//! miner-stats.json reader, worker logs, bounded automatic recovery, and the
//! pool payout poller: what this miner is owed and what it has already been
//! paid, read from the pool itself.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver};
use std::sync::{LazyLock, Mutex, MutexGuard, Once};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use app::efficiency::MiningStatsSnapshot;
use field::{Address, Amount};
use serde_json::Value;

use crate::MinerApp;
use crate::connect::{ConnectMode, is_local_connect, normalize_connect};
use crate::mining_control::clear_worker_stats;
use crate::mining_kind::MiningKind;

const STATS_FILE_POLL_INTERVAL: Duration = Duration::from_millis(350);

/// How old the worker's own snapshot may be before the panel stops drawing the
/// measurements inside it as current.
///
/// The worker withholds a temperature whose sensor has gone quiet, but that rule
/// ends at the file. What the panel reads is a snapshot, not a sensor, and a
/// worker that stops writing leaves the last file it wrote on disk with every
/// number in it intact. It stops writing more often than it looks: a thermal
/// pause, the idle-hours window and the profit pause all return from the mining
/// item before any stats are emitted, and a worker that hung or was killed
/// without the panel noticing never writes again. Aging the snapshot at read
/// time is the only place that can be caught.
///
/// The window is the one `fleet.rs` already calls a stale rig, so the panel has
/// a single definition of a snapshot too old to quote.
pub const STATS_STALE_AFTER: Duration = Duration::from_secs(30);

/// A snapshot stamped further ahead than this came from a clock that disagrees
/// with ours, not from the future. Its age cannot be measured, so it is not
/// quoted either.
const STATS_FUTURE_SKEW: Duration = Duration::from_secs(30);

/// Whether the worker's snapshot is recent enough for the figures in it to be
/// drawn as live measurements. A snapshot that never arrived (`updated_unix_ms`
/// is 0, which is also what the panel resets to when a worker stops) is not.
pub fn snapshot_is_live(stats: &MiningStatsSnapshot, now_ms: u64) -> bool {
    let taken_at = stats.updated_unix_ms;
    if taken_at == 0 || taken_at > now_ms.saturating_add(STATS_FUTURE_SKEW.as_millis() as u64) {
        return false;
    }
    now_ms.saturating_sub(taken_at) <= STATS_STALE_AFTER.as_millis() as u64
}

/// The GPU temperature the panel is allowed to put in the gauge.
///
/// `None` means exactly what an absent reading has always meant here: no
/// temperature to draw, so the ring is empty and says "not measured". It is
/// never a zero and never the last number a dead worker happened to leave
/// behind. The range check is the worker's own (`mining_stats::sensor_temperature`),
/// applied again because what arrives here is a file rather than a sensor.
pub fn live_gpu_temp_c(stats: &MiningStatsSnapshot, now_ms: u64) -> Option<f32> {
    if !snapshot_is_live(stats, now_ms) {
        return None;
    }
    stats
        .gpu_temp_c
        .filter(|c| c.is_finite() && *c > 0.0 && *c < 120.0)
}

/// The OOM clamp in force, or `None` when nothing was clamped.
///
/// `oom_allowed_work_groups` is a COUNT of what the OOM fallback allows, not a
/// count of what was taken away, and on a rig that has never hit an
/// out-of-memory batch it equals `configured_work_groups`. Testing it for `> 0`
/// therefore reports an OOM clamp on every healthy GPU in the world, which is
/// exactly what painted the Hardware ring red on a rig running all 48 of its 48
/// work groups. A clamp exists only where the allowance falls BELOW what the
/// panel configured.
pub fn oom_clamp(stats: &MiningStatsSnapshot) -> Option<u32> {
    let allowed = stats.oom_allowed_work_groups;
    let configured = stats.configured_work_groups;
    // With no configured count there is nothing to be below, so no clamp can be
    // proven and none is claimed.
    if allowed == 0 || configured == 0 || allowed >= configured {
        return None;
    }
    Some(allowed)
}

/// The thermal cap in force, or `None` when the thermal guard has capped
/// nothing. Zero here really does mean "no cap", but a cap at or above the
/// configured count still took nothing away, so it is not reported as a clamp.
pub fn thermal_clamp(stats: &MiningStatsSnapshot) -> Option<u32> {
    let cap = stats.thermal_cap_work_groups;
    let configured = stats.configured_work_groups;
    if cap == 0 || (configured > 0 && cap >= configured) {
        return None;
    }
    Some(cap)
}

/// Whether something is holding this rig below the work groups it was told to
/// run. A stale snapshot proves nothing about now, so it reports nothing.
pub fn work_groups_clamped(stats: &MiningStatsSnapshot, now_ms: u64) -> bool {
    snapshot_is_live(stats, now_ms)
        && (oom_clamp(stats).is_some() || thermal_clamp(stats).is_some())
}

/// How many times a crashed worker is restarted before the panel stops and says
/// so. Public because the sidebar prints the retries spent out of this budget,
/// and a hard-coded "3" there would quietly start lying the day this changes.
pub const MAX_RESTART_ATTEMPTS: u8 = 3;

impl MinerApp {
    pub(super) fn poll_stats(&mut self) {
        // The Master Panel is drawn by `FleetState`, which the frame loop hands
        // a `Ui` and a stats snapshot and no language. This runs once per frame
        // before any screen is drawn, so it is where the chosen language is
        // published for the parts of the UI that cannot be passed it.
        crate::i18n::note_active_lang(self.lang);
        // Money first, and before every early return below: a miner must be able
        // to see what it is owed whether or not the worker is currently running.
        self.poll_pool_money();
        self.poll_worker_stop();
        self.poll_pending_start();
        self.poll_benchmark();
        let now = Instant::now();
        if (self.mining || self.benchmark_operation_active()) && now >= self.stats_next_read {
            self.stats_next_read = now + STATS_FILE_POLL_INTERVAL;
            if let Ok(data) = std::fs::read_to_string(&self.stats_path) {
                if let Ok(s) = serde_json::from_str::<MiningStatsSnapshot>(&data) {
                    self.stats = s;
                }
            }
        }
        self.fold_into_history();

        self.poll_scheduled_restart();
        if !self.mining {
            return;
        }

        self.absorb_worker_lines();

        let exit = match self.child.as_mut().map(|child| child.try_wait()) {
            Some(Ok(Some(exit))) => Some(Ok(exit)),
            Some(Err(error)) => {
                // Do not schedule a replacement while the old process may
                // still be alive. Reap it asynchronously and require a manual
                // start after the OS status-query failure.
                if let Some(child) = self.child.take() {
                    self.worker_stop_rx =
                        Some(crate::mining_control::queue_child_termination(child));
                    self.worker_stop_failed = false;
                }
                self.mining = false;
                self.log_rx = None;
                self.worker_started_at = None;
                self.restart_worker = None;
                clear_worker_stats(&mut self.stats, &self.stats_path);
                self.status_msg = format!(
                    "Worker status check failed: {error}. Automatic retry was disabled to avoid starting a duplicate miner."
                );
                return;
            }
            _ => None,
        };
        let Some(exit) = exit else {
            return;
        };

        // The worker has gone. Read its last words now, without waiting out the
        // usual interval and before the drain above stops running: whatever it
        // printed on the way out is exactly what belongs in the message about
        // why it stopped.
        crate::worker_log_tail::read_immediately();
        self.absorb_worker_lines();

        self.mining = false;
        self.child = None;
        self.log_rx = None;
        clear_worker_stats(&mut self.stats, &self.stats_path);
        let ran_for = self
            .worker_started_at
            .take()
            .map(|started| started.elapsed())
            .unwrap_or_default();
        if ran_for >= Duration::from_secs(300) {
            self.restart_attempts = 0;
        }

        let t = self.t();
        let exit_text = match exit {
            Ok(status) => status.to_string(),
            Err(error) => error,
        };
        let detail = if self.last_worker_log.is_empty() {
            exit_text
        } else {
            format!("{exit_text}: {}", self.last_worker_log)
        };

        if self.restart_attempts < MAX_RESTART_ATTEMPTS {
            self.restart_attempts += 1;
            let delay = Duration::from_secs(3 * self.restart_attempts as u64);
            let worker = match self.mining_kind {
                MiningKind::Hac => self.poworker_path.clone(),
                MiningKind::Hacd => self.diaworker_path.clone(),
            };
            self.restart_worker = Some((worker, Instant::now() + delay));
            self.status_msg = format!(
                "{}: {}. Automatic retry {}/{} in {}s.",
                t.miner_exited,
                detail,
                self.restart_attempts,
                MAX_RESTART_ATTEMPTS,
                delay.as_secs()
            );
        } else {
            self.restart_worker = None;
            self.status_msg = format!(
                "{}: {}. Automatic recovery stopped after {MAX_RESTART_ATTEMPTS} attempts.",
                t.miner_exited, detail
            );
        }
    }

    /// Keep the chart's history, which lives here rather than in the view.
    ///
    /// This runs every frame, on every screen. The dashboard is not the only
    /// place the panel spends its time, and a history that only advanced while
    /// that one tab was open would put an hour-long hole in the chart for every
    /// hour spent in Setup. The freshness rules inside `Series::record` decide
    /// what is actually kept, and they refuse a snapshot the worker has not
    /// rewritten, so a frame that measured nothing records nothing.
    ///
    /// The file beside it is what makes the wider ranges mean anything after a
    /// restart. It is read once, before the first live reading is taken, and
    /// written back on its own slow interval.
    fn fold_into_history(&self) {
        let path = crate::history::path_beside_stats(&self.stats_path);
        let now_ms = now_unix_ms();
        let mut live = crate::dashboard::live();
        crate::history::restore_once(&path, &mut live.series, now_ms);
        live.observe(&self.stats);
        crate::history::persist_if_due(&path, &live.series, now_ms);
    }

    /// Take every line the worker has printed since the last call and file it:
    /// into the dashboard's event feed, into `last_worker_log`, and into the
    /// status line when the worker said something the panel must repeat.
    ///
    /// The output reaches the panel one of two ways, and exactly one of them is
    /// live per platform. Where a worker is piped, the channel carries its
    /// lines. Where the worker owns its own console window instead (Windows) it
    /// pipes nothing back and that channel was born disconnected, so the rolling
    /// log file the worker writes beside its config is the only copy the panel
    /// can read. Reading both on the platform where both work would show every
    /// line twice, so each is read only where it is the real source.
    fn absorb_worker_lines(&mut self) {
        let mut lines: Vec<String> = Vec::new();
        if let Some(rx) = &self.log_rx {
            while let Ok(line) = rx.try_recv() {
                // The Logs screen reads one bounded ring. A piped line is not in
                // it yet, so it is put there here; a tailed line already is.
                crate::worker_log_tail::record_piped(&line);
                lines.push(line);
            }
        }
        #[cfg(windows)]
        lines.extend(
            crate::worker_log_tail::poll()
                .into_iter()
                .map(|line| line.text),
        );
        if lines.is_empty() {
            return;
        }

        let t = self.t();
        for line in lines {
            // The dashboard's event feed has to see every line here. It is the
            // only place they exist: `last_worker_log` keeps one, and a view
            // sampling that between frames would miss whatever was overwritten
            // in between.
            crate::dashboard::record_worker_line(&line);
            if !line.trim().is_empty() {
                self.last_worker_log = line.clone();
            }
            if line.contains("MINING SUCCESS") {
                self.status_msg = t.block_found.to_string();
            } else if line.contains("cannot get block data") {
                self.status_msg = t.fullnode_not_ready.to_string();
            } else if line.contains("OpenCL error")
                || line.contains("GPU batch failed")
                || line.contains("CL_OUT_OF")
            {
                self.status_msg = format!("{} {line}", t.worker_error_prefix);
            }
        }
    }

    fn poll_worker_stop(&mut self) {
        let result = match self.worker_stop_rx.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => result,
            Some(Err(mpsc::TryRecvError::Empty)) | None => return,
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                Err("worker reaper stopped unexpectedly".to_string())
            }
        };
        self.worker_stop_rx = None;
        match result {
            Ok(()) => {
                self.worker_stop_failed = false;
                if self.status_msg == "Stopping miner safely..." {
                    self.status_msg = self.t().mining_stopped.to_string();
                } else {
                    self.status_msg.push_str(" Worker stopped safely.");
                }
            }
            Err(error) => {
                self.worker_stop_failed = true;
                self.status_msg = format!(
                    "Worker stop could not be confirmed: {error}. End the worker process, then restart the panel."
                );
            }
        }
    }

    fn poll_scheduled_restart(&mut self) {
        if self.mining
            || self.pending_start.is_some()
            || self.worker_stopping()
            || self.worker_stop_needs_restart()
            || self.benchmark_operation_active()
            || self.opencl_probe_active()
        {
            return;
        }
        let ready = self
            .restart_worker
            .as_ref()
            .map(|(_, when)| Instant::now() >= *when)
            .unwrap_or(false);
        if !ready {
            return;
        }
        if let Some((worker, _)) = self.restart_worker.take() {
            self.launch_worker(worker);
        }
    }

    pub(super) fn format_stats_age(ms: u64) -> String {
        if ms == 0 {
            return "-".to_string();
        }
        let now_ms = now_unix_ms();
        let sec = now_ms.saturating_sub(ms) / 1000;
        format_age_secs(sec)
    }

    /// Tell the payout poller what to ask about. Runs on the UI thread every
    /// frame and does no I/O at all: it only writes the current pool address and
    /// payout address into a slot the background thread reads.
    fn poll_pool_money(&mut self) {
        let request = self.pool_money_request();
        {
            let mut shared = pool_money_lock();
            if shared.request != request {
                shared.request = request.clone();
                // Nothing we knew about the previous endpoint applies to a new
                // one. Drop it rather than show yesterday's money.
                shared.money = PoolMoney::default();
            }
        }
        if request.is_some() {
            start_pool_money_thread();
        }
    }

    /// What the panel may honestly say about pool money right now. Cheap: it
    /// copies the last answer the background thread published.
    pub(super) fn pool_money(&self) -> PoolMoney {
        // HACD diamonds are mined by the full node against its own configured
        // reward address; no pool in this project pays for them, so there is no
        // ledger to show and no number to invent.
        if self.mining_kind != MiningKind::Hac {
            return PoolMoney::default();
        }
        let Some(request) = self.pool_money_request() else {
            return PoolMoney::default();
        };
        let shared = pool_money_lock();
        if shared.answered_for.as_ref() != Some(&request) {
            return PoolMoney {
                view: PayoutView::Waiting,
                ..PoolMoney::default()
            };
        }
        shared.money.clone()
    }

    /// The endpoint to ask, plus the payout address the worker actually
    /// announces to it. Mirrors `config.rs`, which writes `pool_worker` only in
    /// Pool mode: an address the worker never sends is an address the pool
    /// cannot credit, so the panel must not pretend otherwise.
    fn pool_money_request(&self) -> Option<PoolQuery> {
        if self.mining_kind != MiningKind::Hac {
            return None;
        }
        let connect = normalize_connect(&self.connect).ok()?;
        let worker = match self.connect_mode {
            ConnectMode::Pool => self.wallet.trim().to_string(),
            ConnectMode::Solo => String::new(),
        };
        Some(PoolQuery {
            connect,
            worker: payout_address(&worker).unwrap_or_default(),
            worker_set: !worker.is_empty(),
        })
    }
}

// ---------------------------------------------------------------------------
// Pool payouts
// ---------------------------------------------------------------------------

/// The unit the pool ledger counts in: 0.1 HAC, the Hacash amount `1:247`.
/// The HBIT pool documents and uses exactly this ("units of 0.1 HAC" in
/// `balance_units` / `block_reward_units`), so a `*_units` field coming back
/// from `/earnings` is converted with this unit and nothing else.
const POOL_LEDGER_UNIT: u8 = 247;
/// This is money, not telemetry. Every few seconds is plenty, and it keeps the
/// panel from behaving like a load generator against a pool serving many
/// miners.
const EARNINGS_POLL_INTERVAL: Duration = Duration::from_secs(6);
/// Terms change when an operator reconfigures the pool, which is rare.
const TERMS_POLL_INTERVAL: Duration = Duration::from_secs(300);
/// How often the background thread looks for a new request while idle.
const POLLER_TICK: Duration = Duration::from_millis(500);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Absolute budget for one request. A per-read timeout bounds each syscall, not
/// the whole exchange, so a peer dribbling bytes would never trip it.
const HTTP_WALL_TIMEOUT: Duration = Duration::from_secs(6);
/// Longest answer accepted. An earnings or terms body is a few hundred bytes.
const HTTP_MAX_RESPONSE: usize = 64 * 1024;
/// Longest pool-authored message the panel will repeat back to the user.
const POOL_MESSAGE_MAX: usize = 200;

/// One payout the pool says it has already made.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LastPayout {
    /// `None` when the pool reported a payout without an amount. Never zero.
    pub amount: Option<Amount>,
    pub tx: String,
    pub unix_ms: u64,
}

/// One worker's ledger as the pool reports it. Every money field is optional on
/// purpose: a pool that does not report a number must read as "not reported",
/// never as 0.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PoolEarnings {
    pub worker: String,
    pub paid: Option<Amount>,
    pub pending: Option<Amount>,
    pub in_flight: Option<Amount>,
    pub last_payout: Option<LastPayout>,
    pub shares_in_window: Option<u64>,
}

/// The pool's own terms, read out of the pool rather than out of a promise in
/// this panel. Every field is optional for the same reason as above.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PoolTerms {
    pub scheme: String,
    pub window: Option<u64>,
    pub share_factor: Option<u64>,
    pub fee: String,
    pub minimum_payout: Option<Amount>,
    pub maturity_depth: Option<u64>,
    pub settle_interval_secs: Option<u64>,
}

impl PoolTerms {
    fn is_empty(&self) -> bool {
        *self == PoolTerms::default()
    }
}

/// What the panel is allowed to say about pool money. Each variant is a
/// different fact, and none of them may be rendered as "0 HAC": a miner reading
/// zero when the truth is "unknown" or "this is not a payout pool" has been
/// misled.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PayoutView {
    /// Nothing is configured to ask, or this is not HAC block mining.
    #[default]
    NotApplicable,
    /// The first answer has not arrived yet.
    Waiting,
    /// A plain Hacash full node, identified by its own `/_server_` banner. It
    /// keeps no per miner ledger: block rewards go to that node's reward
    /// wallet.
    FullNode { local: bool },
    /// hac-pool, identified by its own `/_server_` banner. It relays work and
    /// keeps no wallet and no share record, so it never pays miners.
    Relay,
    /// A payout pool that does not publish per miner earnings.
    PoolWithoutEarnings,
    /// Reachable, but it published neither earnings, nor terms, nor a banner we
    /// recognise. The panel will not guess what it pays.
    Unidentified,
    /// No payout address reaches the pool, so it has nothing to credit.
    /// `invalid` distinguishes "you typed something unusable" from "empty".
    NoPayoutAddress { invalid: bool },
    /// The pool answered the earnings request with its own refusal. Its text is
    /// repeated verbatim (sanitised) because it usually names the fix.
    PoolRefused(String),
    /// Could not reach or could not read the endpoint. UNKNOWN, not zero.
    /// `was_pool` records whether this endpoint had already answered as a
    /// payout pool: only then may the panel say a POOL is unreachable. An
    /// address that never answered might be a stopped full node, and calling
    /// that "your pool is down" would be a guess.
    Unreachable { reason: String, was_pool: bool },
    /// The pool answered for this worker.
    Earnings(PoolEarnings),
}

/// A published answer plus the terms that were current when it was taken.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PoolMoney {
    pub view: PayoutView,
    pub terms: Option<PoolTerms>,
    /// When the answer was taken, so the UI can age it. 0 = never.
    pub checked_unix_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PoolQuery {
    connect: String,
    /// Validated payout address, or empty when none can be announced.
    worker: String,
    /// True when the user did set an address, so an empty `worker` above means
    /// "unusable" rather than "not set".
    worker_set: bool,
}

#[derive(Default)]
struct PoolMoneyShared {
    /// What the UI wants polled. `None` = nothing to ask.
    request: Option<PoolQuery>,
    money: PoolMoney,
    /// The query `money` belongs to. An answer is never shown against a pool or
    /// an address the user has since changed.
    answered_for: Option<PoolQuery>,
}

static POOL_MONEY: LazyLock<Mutex<PoolMoneyShared>> =
    LazyLock::new(|| Mutex::new(PoolMoneyShared::default()));
static POOL_MONEY_THREAD: Once = Once::new();

/// Recover from a poisoned mutex instead of taking the panel down with it. The
/// worst case is one stale money reading, which the next poll replaces.
fn pool_money_lock() -> MutexGuard<'static, PoolMoneyShared> {
    POOL_MONEY.lock().unwrap_or_else(|e| e.into_inner())
}

/// The panel talks to exactly one pool at a time, so one poller thread is the
/// whole requirement. It is started on the first request and then parks on a
/// timer; the UI thread never does I/O and never waits on it.
fn start_pool_money_thread() {
    POOL_MONEY_THREAD.call_once(|| {
        let _ = thread::Builder::new()
            .name("pool-money".to_string())
            .spawn(pool_money_loop);
    });
}

fn pool_money_loop() {
    let mut current: Option<PoolQuery> = None;
    let mut terms: Option<PoolTerms> = None;
    // Sticky for as long as one endpoint stays selected: once it has answered
    // as a payout pool, a later outage is that pool being unreachable.
    let mut confirmed_pool = false;
    let mut next_earnings = Instant::now();
    let mut next_terms = Instant::now();
    loop {
        let request = pool_money_lock().request.clone();
        let Some(query) = request else {
            current = None;
            terms = None;
            confirmed_pool = false;
            thread::sleep(POLLER_TICK);
            continue;
        };
        if current.as_ref() != Some(&query) {
            current = Some(query.clone());
            terms = None;
            confirmed_pool = false;
            next_earnings = Instant::now();
            next_terms = Instant::now();
        }
        if Instant::now() < next_earnings {
            thread::sleep(POLLER_TICK);
            continue;
        }
        next_earnings = Instant::now() + EARNINGS_POLL_INTERVAL;

        // No lock is held across any of this: the UI reads the same mutex.
        if terms.is_none() || Instant::now() >= next_terms {
            next_terms = Instant::now() + TERMS_POLL_INTERVAL;
            terms = fetch_terms(&query.connect);
        }
        let view = fetch_payout_view(&query, terms.is_some(), confirmed_pool);
        confirmed_pool |= view_confirms_a_pool(&view);

        let mut shared = pool_money_lock();
        if shared.request.as_ref() != Some(&query) {
            continue; // the user moved on while we were on the wire
        }
        shared.money = PoolMoney {
            view,
            terms: terms.clone(),
            checked_unix_ms: now_unix_ms(),
        };
        shared.answered_for = Some(query);
    }
}

/// An answer that proves the other end really is a payout pool.
fn view_confirms_a_pool(view: &PayoutView) -> bool {
    matches!(
        view,
        PayoutView::Earnings(_)
            | PayoutView::PoolRefused(_)
            | PayoutView::PoolWithoutEarnings
            | PayoutView::NoPayoutAddress { .. }
    )
}

/// Ask the endpoint what this worker is owed, and when it cannot answer, work
/// out what the endpoint actually is so the panel says the right thing rather
/// than a zero.
fn fetch_payout_view(query: &PoolQuery, has_terms: bool, was_pool: bool) -> PayoutView {
    let unreachable = |reason: String| PayoutView::Unreachable {
        reason,
        was_pool: was_pool || has_terms,
    };
    if !query.worker.is_empty() {
        match http_get(
            &query.connect,
            &format!("/earnings?worker={}", query.worker),
        ) {
            Err(error) => return unreachable(error),
            Ok((200, body)) => match parse_earnings(&body, &query.worker) {
                EarningsAnswer::Earnings(e) => return PayoutView::Earnings(e),
                EarningsAnswer::Refused(message) => return PayoutView::PoolRefused(message),
                EarningsAnswer::NotEarnings => {}
            },
            Ok(_) => {}
        }
    }

    let identity = classify_endpoint(&query.connect);
    match identity {
        Endpoint::Relay => return PayoutView::Relay,
        Endpoint::FullNode => {
            return PayoutView::FullNode {
                local: is_local_connect(&query.connect),
            };
        }
        Endpoint::Unreachable(error) => return unreachable(error),
        Endpoint::Other => {}
    }

    // Establish that it IS a payout pool before saying anything that presumes
    // one: it must publish terms, or the share window a pool-server that
    // predates /earnings still puts on /stats.
    if !(has_terms || publishes_share_window(&query.connect)) {
        return PayoutView::Unidentified;
    }
    if query.worker.is_empty() {
        return PayoutView::NoPayoutAddress {
            invalid: query.worker_set,
        };
    }
    PayoutView::PoolWithoutEarnings
}

enum Endpoint {
    /// hac-pool relay.
    Relay,
    /// Plain Hacash full node.
    FullNode,
    /// Answered, but not with a banner we know.
    Other,
    Unreachable(String),
}

/// Both hac-pool and the full node answer `/_server_` with a fixed string of
/// their own (`miner-pool/src/rpc_proxy.rs` and `server/src/server/route.rs`),
/// which is the only identification either of them offers.
fn classify_endpoint(connect: &str) -> Endpoint {
    match http_get(connect, "/_server_") {
        Err(error) => Endpoint::Unreachable(error),
        Ok((status, body)) => {
            if status == 200 && body.contains("Hacash Pool") {
                Endpoint::Relay
            } else if status == 200 && body.contains("Hacash Api Server") {
                Endpoint::FullNode
            } else {
                Endpoint::Other
            }
        }
    }
}

/// A pool-server that predates `/earnings` still publishes its PPLNS share
/// window on `/stats`. That proves it is a payout pool, which is a different
/// (and much better) message than "unknown endpoint".
fn publishes_share_window(connect: &str) -> bool {
    match http_get(connect, "/stats") {
        Ok((200, body)) => json_value(&body)
            .map(|v| v.get("share_window").is_some() || v.get("workers").is_some())
            .unwrap_or(false),
        _ => false,
    }
}

fn fetch_terms(connect: &str) -> Option<PoolTerms> {
    let (status, body) = http_get(connect, "/terms").ok()?;
    if status != 200 {
        return None;
    }
    parse_terms(&body)
}

enum EarningsAnswer {
    Earnings(PoolEarnings),
    /// The pool answered, and said no. Its wording usually names the fix.
    Refused(String),
    /// Not an earnings answer at all (a 404 body, a full node's JSON, ...).
    NotEarnings,
}

fn parse_earnings(body: &str, worker: &str) -> EarningsAnswer {
    let Some(v) = json_value(body) else {
        return EarningsAnswer::NotEarnings;
    };
    if v.get("ok").and_then(Value::as_bool) == Some(false) {
        return match pool_message(&v) {
            Some(message) => EarningsAnswer::Refused(message),
            None => EarningsAnswer::NotEarnings,
        };
    }
    // Require at least one field only an earnings answer carries, so a full
    // node's unrelated JSON is never read as an empty ledger.
    let money_fields = [
        "paid",
        "paid_units",
        "pending",
        "pending_units",
        "in_flight",
        "in_flight_units",
        "last_payout",
        "shares_in_window",
    ];
    if !money_fields.iter().any(|f| v.get(*f).is_some()) {
        return EarningsAnswer::NotEarnings;
    }
    EarningsAnswer::Earnings(PoolEarnings {
        worker: v
            .get("worker")
            .and_then(Value::as_str)
            .unwrap_or(worker)
            .to_string(),
        paid: read_amount(&v, "paid"),
        pending: read_amount(&v, "pending"),
        in_flight: read_amount(&v, "in_flight"),
        last_payout: read_last_payout(&v),
        shares_in_window: v.get("shares_in_window").and_then(Value::as_u64),
    })
}

fn read_last_payout(v: &Value) -> Option<LastPayout> {
    let last = v.get("last_payout")?;
    if last.is_null() {
        return None;
    }
    let amount = read_amount(last, "amount").or_else(|| amount_from_units(last.get("units")));
    let tx = last
        .get("tx")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let unix_ms = last
        .get("unix_ms")
        .and_then(Value::as_u64)
        .or_else(|| {
            last.get("unix_sec")
                .and_then(Value::as_u64)
                .map(|s| s * 1000)
        })
        .unwrap_or(0);
    if amount.is_none() && tx.is_empty() && unix_ms == 0 {
        return None;
    }
    Some(LastPayout {
        amount,
        tx: sanitise(&tx, 80),
        unix_ms,
    })
}

/// Read one money field. A chain amount string (`"12:247"`, `"1.2"`) is
/// preferred because it is unambiguous; `<field>_units` is accepted as the
/// pool's own integer ledger unit of 0.1 HAC. Anything unreadable stays `None`
/// so the UI shows "not reported" instead of a number nobody can stand behind.
fn read_amount(v: &Value, field: &str) -> Option<Amount> {
    if let Some(text) = v.get(field).and_then(Value::as_str) {
        if let Ok(amount) = Amount::from(text) {
            return Some(amount);
        }
    }
    amount_from_units(v.get(&format!("{field}_units")))
}

fn amount_from_units(v: Option<&Value>) -> Option<Amount> {
    v.and_then(Value::as_u64).map(units_to_amount)
}

/// The pool's integer ledger unit is 0.1 HAC. Built through the chain's own
/// `Amount`, never through a float and never through a hand written decimal.
pub fn units_to_amount(units: u64) -> Amount {
    Amount::coin(units, POOL_LEDGER_UNIT)
}

fn parse_terms(body: &str) -> Option<PoolTerms> {
    let v = json_value(body)?;
    if v.get("ok").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let terms = PoolTerms {
        scheme: v
            .get("scheme")
            .and_then(Value::as_str)
            .map(|s| sanitise(s, 40))
            .unwrap_or_default(),
        window: v
            .get("window")
            .or_else(|| v.get("pplns_window"))
            .and_then(Value::as_u64),
        share_factor: v.get("share_factor").and_then(Value::as_u64),
        fee: read_fee(&v),
        minimum_payout: read_amount(&v, "minimum_payout").or_else(|| read_amount(&v, "minimum")),
        maturity_depth: v.get("maturity_depth").and_then(Value::as_u64),
        settle_interval_secs: v
            .get("settle_interval_secs")
            .or_else(|| v.get("settle_secs"))
            .and_then(Value::as_u64),
    };
    (!terms.is_empty()).then_some(terms)
}

/// The fee as the pool words it. A pool that takes nothing usually says so with
/// a number, so `0` is turned into an explicit "0%" rather than left blank.
fn read_fee(v: &Value) -> String {
    if let Some(text) = v.get("fee").and_then(Value::as_str) {
        return sanitise(text, 40);
    }
    if let Some(percent) = v
        .get("fee_percent")
        .or_else(|| v.get("fee"))
        .and_then(Value::as_u64)
    {
        return format!("{percent}%");
    }
    String::new()
}

fn pool_message(v: &Value) -> Option<String> {
    let text = v
        .get("err")
        .or_else(|| v.get("error"))
        .or_else(|| v.get("message"))
        .and_then(Value::as_str)?;
    let text = sanitise(text, POOL_MESSAGE_MAX);
    (!text.is_empty()).then_some(text)
}

/// The pool is a remote party. Its text may reach the UI, but only as a single
/// bounded line of printable characters.
fn sanitise(text: &str, max: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    match cleaned.char_indices().nth(max) {
        Some((idx, _)) => cleaned[..idx].trim_end().to_string(),
        None => cleaned,
    }
}

/// Parse a JSON body, tolerating a transfer encoding that wrapped it: take the
/// widest `{...}` span and let serde decide whether it is valid.
fn json_value(body: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(body.trim()) {
        return Some(v);
    }
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    (end > start).then(|| serde_json::from_str::<Value>(&body[start..=end]).ok())?
}

/// A payout address the panel is willing to put in a request line: it must be a
/// Hacash address the chain itself can parse, and its readable form is base58,
/// so it can carry nothing that would change the meaning of the request.
fn payout_address(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Address::from_readable(trimmed).ok()?;
    Some(trimmed.to_string())
}

/// One small blocking HTTP GET with an absolute deadline and a bounded answer.
/// Runs on the poller thread only.
pub(crate) fn http_get(connect: &str, path: &str) -> Result<(u16, String), String> {
    let address = normalize_connect(connect)?;
    let deadline = Instant::now() + HTTP_WALL_TIMEOUT;
    let resolved = address
        .as_str()
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {address}: {e}"))?;

    let mut stream = None;
    let mut last_error = format!("no address resolved for {address}");
    for socket_address in resolved {
        let left = remaining(deadline)?;
        match TcpStream::connect_timeout(&socket_address, left.min(HTTP_CONNECT_TIMEOUT)) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(error) => last_error = error.to_string(),
        }
    }
    let mut stream = stream.ok_or(last_error)?;

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream
        .set_write_timeout(Some(remaining(deadline)?))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;

    let raw = read_response(&mut stream, deadline)?;
    let (status, body) = split_response(&raw).ok_or_else(|| "unreadable answer".to_string())?;
    Ok((status, body))
}

fn remaining(deadline: Instant) -> Result<Duration, String> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err("the pool did not answer in time".to_string());
    }
    Ok(left)
}

fn read_response(stream: &mut TcpStream, deadline: Instant) -> Result<Vec<u8>, String> {
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if response_complete(&raw) {
            return Ok(raw);
        }
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|e| e.to_string())?;
        match stream.read(&mut buf) {
            Ok(0) => return Ok(raw),
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > HTTP_MAX_RESPONSE {
                    return Err("the pool sent an answer that is too large".to_string());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if !raw.is_empty() => {
                let _ = error;
                return Ok(raw);
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// True once the headers name a Content-Length and that many body bytes have
/// arrived, so a peer that keeps the socket open cannot stall us to the
/// deadline.
fn response_complete(raw: &[u8]) -> bool {
    let Some((head, body)) = split_head(raw) else {
        return false;
    };
    match content_length(head) {
        Some(len) => body.len() >= len,
        None => false,
    }
}

fn split_head(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    Some((&raw[..end], &raw[end + 4..]))
}

fn content_length(head: &[u8]) -> Option<usize> {
    let head = std::str::from_utf8(head).ok()?;
    head.lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        })
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// Split an HTTP answer into (status code, body). Lossy on the body so a pool
/// answering with bytes that are not UTF-8 fails the JSON parse instead of the
/// whole poll.
fn split_response(raw: &[u8]) -> Option<(u16, String)> {
    let (head, body) = split_head(raw)?;
    let head = std::str::from_utf8(head).ok()?;
    let status = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some((status, String::from_utf8_lossy(body).to_string()))
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Compact relative age, in the same shape the dashboard already uses for the
/// stats file.
pub fn format_age_secs(sec: u64) -> String {
    if sec < 60 {
        format!("{sec}s")
    } else if sec < 3600 {
        format!("{}m", sec / 60)
    } else if sec < 86_400 {
        format!("{}h", sec / 3600)
    } else {
        format!("{}d", sec / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The operator's live rig this minute: 48 configured, 48 allowed, no
    /// thermal cap, snapshot fresh.
    fn healthy_rig() -> MiningStatsSnapshot {
        MiningStatsSnapshot {
            configured_work_groups: 48,
            oom_allowed_work_groups: 48,
            thermal_cap_work_groups: 0,
            effective_work_groups: 48,
            updated_unix_ms: now_unix_ms(),
            ..Default::default()
        }
    }

    #[test]
    fn a_rig_running_every_work_group_it_was_given_is_not_clamped() {
        // This is the whole defect: `oom_allowed_work_groups` is a COUNT, and
        // reading it as "work groups lost to an OOM" turned the Hardware ring
        // red on a perfectly healthy card mining mainnet.
        let stats = healthy_rig();
        assert_eq!(oom_clamp(&stats), None);
        assert_eq!(thermal_clamp(&stats), None);
        assert!(!work_groups_clamped(&stats, now_unix_ms()));
    }

    #[test]
    fn a_clamp_is_reported_only_where_the_allowance_falls_short() {
        let mut stats = healthy_rig();
        stats.oom_allowed_work_groups = 24;
        stats.effective_work_groups = 24;
        assert_eq!(oom_clamp(&stats), Some(24));
        assert!(work_groups_clamped(&stats, now_unix_ms()));

        // A thermal cap at or above the configured count took nothing away.
        let mut capped = healthy_rig();
        capped.thermal_cap_work_groups = 48;
        assert_eq!(thermal_clamp(&capped), None);
        assert!(!work_groups_clamped(&capped, now_unix_ms()));
        capped.thermal_cap_work_groups = 12;
        assert_eq!(thermal_clamp(&capped), Some(12));
        assert!(work_groups_clamped(&capped, now_unix_ms()));
    }

    #[test]
    fn a_worker_that_reported_nothing_is_never_called_clamped() {
        // No configured count and no allowance means nothing is known, and an
        // unknown is not a fault. A stale snapshot proves nothing about now
        // either, however alarming the numbers frozen in it.
        let mut nothing = MiningStatsSnapshot {
            updated_unix_ms: now_unix_ms(),
            ..Default::default()
        };
        assert_eq!(oom_clamp(&nothing), None);
        assert!(!work_groups_clamped(&nothing, now_unix_ms()));
        nothing.configured_work_groups = 48;
        assert_eq!(oom_clamp(&nothing), None, "0 allowed is unreported, not 0");

        let stale = MiningStatsSnapshot {
            configured_work_groups: 48,
            oom_allowed_work_groups: 4,
            updated_unix_ms: now_unix_ms() - (STATS_STALE_AFTER.as_millis() as u64 + 1_000),
            ..Default::default()
        };
        assert_eq!(
            oom_clamp(&stale),
            Some(4),
            "the file still says what it says"
        );
        assert!(
            !work_groups_clamped(&stale, now_unix_ms()),
            "a stale snapshot must not turn the ring red"
        );
    }

    #[test]
    fn pool_units_are_tenths_of_a_hac_in_the_chains_own_amount() {
        // the HBIT pool counts in units of 0.1 HAC: "1:247" is one unit and
        // "1:248" is 1 HAC. Getting this wrong is a factor of ten on real money.
        assert_eq!(units_to_amount(1).to_fin_string(), "1:247");
        assert_eq!(units_to_amount(12).to_fin_string(), "12:247");
        assert_eq!(units_to_amount(10).to_fin_string(), "1:248"); // 1 HAC
        assert_eq!(units_to_amount(250).to_fin_string(), "25:248"); // 25 HAC
        assert_eq!(units_to_amount(2500).to_fin_string(), "25:249"); // 250 HAC
        assert!(units_to_amount(0).is_zero());
    }

    #[test]
    fn earnings_prefer_the_chain_amount_string_and_accept_ledger_units() {
        let body = r#"{"ok":true,"worker":"1abc","paid":"25:248","pending_units":7,
            "in_flight_units":0,"shares_in_window":42}"#;
        let EarningsAnswer::Earnings(e) = parse_earnings(body, "1abc") else {
            panic!("a well formed earnings answer must parse");
        };
        assert_eq!(e.paid.unwrap().to_fin_string(), "25:248");
        assert_eq!(e.pending.unwrap().to_fin_string(), "7:247");
        // A pool-confirmed zero is a real answer and must survive as an Amount,
        // so the UI can say "nothing owed" rather than "not reported".
        assert!(e.in_flight.unwrap().is_zero());
        assert_eq!(e.shares_in_window, Some(42));
        assert_eq!(e.worker, "1abc");
    }

    #[test]
    fn a_field_the_pool_did_not_send_stays_unknown_and_never_becomes_zero() {
        let body = r#"{"ok":true,"worker":"1abc","paid_units":30}"#;
        let EarningsAnswer::Earnings(e) = parse_earnings(body, "1abc") else {
            panic!("earnings must parse");
        };
        assert_eq!(e.paid.unwrap().to_fin_string(), "3:248");
        assert_eq!(e.pending, None, "an absent pending is unknown, not zero");
        assert_eq!(e.in_flight, None);
        assert_eq!(e.last_payout, None);
        assert_eq!(e.shares_in_window, None);
    }

    #[test]
    fn unrelated_json_is_never_read_as_an_empty_ledger() {
        // A full node answering something unrelated on /earnings must not turn
        // into "you have been paid nothing".
        for body in [
            r#"{"ret":0,"height":123}"#,
            r#"{"ok":true}"#,
            "not json at all",
            "",
        ] {
            assert!(
                matches!(parse_earnings(body, "1abc"), EarningsAnswer::NotEarnings),
                "must not be read as earnings: {body}"
            );
        }
    }

    #[test]
    fn a_refusal_is_repeated_to_the_miner_instead_of_being_swallowed() {
        let body =
            r#"{"ok":false,"err":"set pool_worker=<your HAC address> so the pool can pay you"}"#;
        let EarningsAnswer::Refused(message) = parse_earnings(body, "1abc") else {
            panic!("a refusal must reach the miner");
        };
        assert!(message.contains("pool_worker"));
    }

    #[test]
    fn last_payout_keeps_amount_transaction_and_time() {
        let body = r#"{"ok":true,"paid_units":30,"last_payout":
            {"units":20,"tx":"ab12","unix_ms":1720000000000}}"#;
        let EarningsAnswer::Earnings(e) = parse_earnings(body, "1abc") else {
            panic!("earnings must parse");
        };
        let last = e.last_payout.expect("last payout");
        assert_eq!(last.amount.unwrap().to_fin_string(), "2:248");
        assert_eq!(last.tx, "ab12");
        assert_eq!(last.unix_ms, 1_720_000_000_000);
    }

    #[test]
    fn a_pool_that_never_paid_reports_no_last_payout_rather_than_a_blank_one() {
        let body = r#"{"ok":true,"paid_units":0,"last_payout":null}"#;
        let EarningsAnswer::Earnings(e) = parse_earnings(body, "1abc") else {
            panic!("earnings must parse");
        };
        assert!(e.paid.unwrap().is_zero());
        assert_eq!(e.last_payout, None);
    }

    #[test]
    fn terms_are_read_from_the_pool_and_a_zero_fee_is_stated_not_hidden() {
        let body = r#"{"scheme":"PPLNS","window":4096,"share_factor":24,"fee_percent":0,
            "minimum_payout_units":1,"maturity_depth":16,"settle_interval_secs":300}"#;
        let terms = parse_terms(body).expect("terms must parse");
        assert_eq!(terms.scheme, "PPLNS");
        assert_eq!(terms.window, Some(4096));
        assert_eq!(terms.share_factor, Some(24));
        assert_eq!(terms.fee, "0%");
        assert_eq!(terms.minimum_payout.unwrap().to_fin_string(), "1:247");
        assert_eq!(terms.maturity_depth, Some(16));
        assert_eq!(terms.settle_interval_secs, Some(300));
    }

    #[test]
    fn an_endpoint_without_terms_publishes_none() {
        assert_eq!(
            parse_terms(r#"{"ok":false,"err":"no such endpoint"}"#),
            None
        );
        assert_eq!(parse_terms(r#"{"ret":0,"height":9}"#), None);
        assert_eq!(parse_terms("nonsense"), None);
    }

    #[test]
    fn only_an_address_the_chain_can_parse_is_ever_put_in_a_request() {
        assert_eq!(
            payout_address(" 1NVYv5jmr9JRF3usPZJQmJFJhbQhrPESTP "),
            Some("1NVYv5jmr9JRF3usPZJQmJFJhbQhrPESTP".to_string())
        );
        assert_eq!(payout_address(""), None);
        assert_eq!(payout_address("not-an-address"), None);
        // Nothing that could rewrite the request line survives.
        assert_eq!(payout_address("1abc&worker=1eve"), None);
        assert_eq!(payout_address("1abc HTTP/1.1"), None);
        assert_eq!(payout_address("1abc\r\nX: y"), None);
    }

    #[test]
    fn pool_text_reaches_the_ui_as_one_bounded_printable_line() {
        assert_eq!(sanitise("  a\r\n b\tc  ", 80), "a b c");
        assert_eq!(sanitise(&"x".repeat(500), 10), "xxxxxxxxxx");
        assert_eq!(sanitise("καλά", 80), "καλά");
    }

    #[test]
    fn http_answers_are_split_into_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        assert_eq!(split_response(raw), Some((200, "{\"a\":1}".to_string())));
        assert!(response_complete(raw));

        let short = b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n{\"a\"";
        assert!(!response_complete(short));

        let missing = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(split_response(missing), Some((404, String::new())));
        assert_eq!(split_response(b"garbage"), None);
    }

    #[test]
    fn a_body_wrapped_by_a_transfer_encoding_still_parses() {
        let chunked = "7\r\n{\"a\":1}\r\n0\r\n\r\n";
        assert!(json_value(chunked).is_some());
    }

    fn snapshot_at(taken_at_ms: u64, temp: Option<f32>) -> MiningStatsSnapshot {
        MiningStatsSnapshot {
            gpu_temp_c: temp,
            updated_unix_ms: taken_at_ms,
            ..Default::default()
        }
    }

    #[test]
    fn a_temperature_the_worker_stopped_refreshing_is_not_drawn_as_current() {
        // The failure this exists for: the worker pauses on temperature, returns
        // from the mining item before writing any stats, and the file on disk
        // keeps its last reading forever. Nothing about that file changes, so
        // the age has to be taken here or not at all.
        let now = 1_800_000_000_000u64;
        let stale = STATS_STALE_AFTER.as_millis() as u64;

        assert_eq!(
            live_gpu_temp_c(&snapshot_at(now - 1_000, Some(71.5)), now),
            Some(71.5),
            "a snapshot written a second ago is a measurement"
        );
        assert_eq!(
            live_gpu_temp_c(&snapshot_at(now - stale, Some(71.5)), now),
            Some(71.5),
            "the window itself is still inside the window"
        );
        assert_eq!(
            live_gpu_temp_c(&snapshot_at(now - stale - 1, Some(71.5)), now),
            None,
            "past it there is no reading, not a frozen one"
        );
        assert_eq!(
            live_gpu_temp_c(&snapshot_at(now - 3_600_000, Some(71.5)), now),
            None,
            "an hour-old file must not put a confident number on the gauge"
        );
    }

    #[test]
    fn an_absent_or_impossible_temperature_stays_absent_and_never_becomes_zero() {
        let now = 1_800_000_000_000u64;
        assert_eq!(live_gpu_temp_c(&snapshot_at(now, None), now), None);
        for impossible in [0.0, -12.0, 120.0, 5_000.0, f32::NAN, f32::INFINITY] {
            assert_eq!(
                live_gpu_temp_c(&snapshot_at(now, Some(impossible)), now),
                None,
                "{impossible} is not a GPU temperature"
            );
        }
    }

    #[test]
    fn a_snapshot_that_never_arrived_or_came_from_a_disagreeing_clock_is_not_live() {
        let now = 1_800_000_000_000u64;
        // What the panel holds before the first read, and what it resets to when
        // a worker stops.
        assert!(!snapshot_is_live(&MiningStatsSnapshot::default(), now));
        assert!(!snapshot_is_live(&snapshot_at(0, Some(60.0)), now));
        // A small skew is a clock, not a lie; a large one cannot be aged at all.
        assert!(snapshot_is_live(&snapshot_at(now + 1_000, Some(60.0)), now));
        assert!(!snapshot_is_live(
            &snapshot_at(now + 10 * 60 * 1_000, Some(60.0)),
            now
        ));
        assert!(snapshot_is_live(&snapshot_at(now - 1, Some(60.0)), now));
    }

    #[test]
    fn relative_age_stays_readable_past_a_day() {
        assert_eq!(format_age_secs(5), "5s");
        assert_eq!(format_age_secs(90), "1m");
        assert_eq!(format_age_secs(7200), "2h");
        assert_eq!(format_age_secs(200_000), "2d");
    }
}
