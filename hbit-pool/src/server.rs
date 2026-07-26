//! Hacash pool server: serves work, validates shares, keeps PPLNS accounting,
//! submits full blocks, and settles payouts. Blocking HTTP on std::net — no
//! async runtime, no node changes.
//!
//! Speaks the STANDARD miner API (so an unmodified poworker can mine here) with
//! one difference: `target_hash` carries the pool's SHARE target. A submission
//! is promoted to a real block whenever it also beats the network target.
//!
//! Protections that matter once other people's hashrate is involved:
//!   * duplicate shares are rejected (a resubmitted solution cannot inflate a
//!     miner's PPLNS credit at everyone else's expense)
//!   * accounting is persisted atomically, so a restart never erases work
//!   * a submitted block only counts once the chain still holds OUR hash at that
//!     height — orphans are detected and not paid for
//!   * a found block's coinbase is held back from settlement until the chain has
//!     buried it, so a reorg can never revoke income that was already paid out
//!   * only one process may settle a wallet (OS lock), and it shares ONE pending
//!     payout ledger with the manual hbit-pool-payout tool
//!   * settlement runs automatically on a timer, is idempotent across restarts,
//!     and chunks into <=200-action transactions the node will accept
//!   * one panicking request or poisoned lock cannot freeze or crash the pool
//!   * per-IP connection caps + a separate long-poll budget stop one host from
//!     exhausting every connection slot
//!
//! Endpoints: /work, /share, /stats, /terms, /earnings (own protocol) and
//! /query/miner/pending, /query/miner/notice, /submit/miner/success (standard
//! API).
//!
//! /terms and /earnings exist so a miner can judge the pool and see its money:
//!   * /terms states the scheme, window, fee, minimum payout and maturity depths
//!     by READING the constants and live values the pool acts on, so what is
//!     advertised cannot drift from what is done
//!   * /earnings?worker=<address> reports one worker's shares, its PAID total
//!     (confirmed on chain), what is IN FLIGHT (submitted, unconfirmed) and an
//!     explicitly-labelled PENDING estimate. The three are disjoint, and any of
//!     them the pool cannot stand behind is reported as unknown, never as zero.
//!
//! Usage: hbit-pool-server <node> <wallet_file> <listen> <share_bits> <chain> [settle_secs]
//!   `chain` is REQUIRED: a wrong difficulty rule makes every share/block the
//!   node rejects, so there is no silent default. It is `mainnet`, `testnet`, or
//!   `testnet:<difficulty_adjust_blocks>:<each_block_target_time>` for a testnet
//!   node configured with anything other than the documented 288/10 pair. The
//!   choice is PROVED against the node's own tip before the pool serves work.

use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use basis::interface::*;
use field::*;
use protocol::action::HacToTrs;
use protocol::transaction::TransactionType2;
use sys::curtimes;

use hbit_pool::difficulty::ChainParams;
use hbit_pool::pool_core::{self, Pplns, split_payout};
use hbit_pool::{
    Admission, PAYOUT_CHUNK, PAYOUT_DUST_UNITS, PAYOUT_MATURITY_DEPTH, PAYOUT_UNIT, POOL_FEE_UNITS,
    PPLNS_WINDOW, PaidLedger, PayoutRecord, PayoutTxState, SETTLE_RESERVE_UNITS, Template,
    acquire_settle_lock, assemble_block, atomic_write, balance, balance_units, block_reward_units,
    chunk_tx_fee, classify_payout_tx, coinbase_body_hex, coinbase_with_extranonce, confirm_payout,
    distributable_units, drop_payout, fetch_pool_template, find_str, find_u64, get_json,
    http_client, intro_bytes, is_payout_address, load_or_create_wallet, parse_paid_ledger,
    parse_payout_records, payout_amount, pool_state_path, post_hex, submit_block_bytes,
    verify_admitted, verify_chain_params,
};

use serde_json::json;

/// Global live-connection cap and the per-source-IP cap. The per-IP cap is what
/// actually stops one host pinning every slot with long-polls; the global cap is
/// a coarse backstop.
const MAX_CONNS: usize = 1024;
const MAX_PER_IP: u32 = 24;
/// Long-poll waiters are budgeted separately so they can never consume the whole
/// connection pool and starve short /work, /share and /submit requests.
const MAX_NOTICE_WAITERS: usize = 384;
/// Per-height replay-protection set bound: beyond this a template is producing an
/// implausible flood, so reject further shares this height rather than grow memory
/// without limit. Reset every time the height advances.
const SEEN_CAP: usize = 2_000_000;
/// After this many consecutive settlement cycles skipped by a payout that is
/// still sitting in the mempool, escalate from a note to a loud warning: nothing
/// is being paid, and the operator has to know why.
const STALLED_PAYOUT_CYCLES: u64 = 3;
/// How deep one of OUR blocks must be buried before its coinbase may be paid
/// out. The node treats anything shallower than `unstable_block` (4) as
/// reorg-able, and settlement runs at roughly one block interval, so without a
/// generous margin the pool would distribute income that is 0-1 confirmations
/// old. If such a block is later orphaned the income vanishes from the canonical
/// chain while the payout that spent it stays valid, and the operator eats the
/// whole subsidy with no way to recover it.
const COINBASE_MATURITY_DEPTH: u64 = 16;
/// Absolute wall-clock budget for reading a request line. A socket read timeout
/// bounds each read syscall, NOT the whole request, so a client dribbling one
/// byte at a time never trips it; this deadline is what actually stops it.
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(5);
/// Longest request line accepted. A miner API request line is tiny.
const MAX_REQUEST_LINE: usize = 4 * 1024;
/// Stack for a connection handler. HTTP parsing plus one x16rs evaluation needs
/// well under 100 KB, so this trims the address space reserved by MAX_CONNS
/// live handlers without coming close to the real requirement.
const HANDLER_STACK_BYTES: usize = 1024 * 1024;
/// Narrowest and widest share the pool will serve, as powers of two easier than
/// a network block. See `check_share_factor` for why both ends matter.
const MIN_SHARE_FACTOR: u32 = 18;
const MAX_SHARE_FACTOR: u32 = 40;
/// Consecutive above-target submissions from one worker before the pool shouts.
/// A worker that hashes the same header the pool reconstructs essentially never
/// produces one, because it only submits what already beat the target it was
/// handed. A steady stream of them means the two are hashing DIFFERENT headers,
/// and every one of that worker's shares is being thrown away.
const BAD_STREAK_WARN: u64 = 16;
/// Workers tracked by the above-target streak counter. Bounded so a flood of
/// invented worker ids cannot grow memory; the diagnostic is for real miners.
const BAD_STREAK_WORKERS: usize = 4096;
/// How long the "mining without the node's transactions" warning is suppressed
/// after being printed with the same reason. The template loop runs every couple
/// of seconds, so without this the warning would be the whole log.
const TX_WARN_REPEAT: Duration = Duration::from_secs(300);
/// Template cycles between refreshes of the pool's own wallet valuation. That
/// number is what `/earnings` divides into a worker's PENDING estimate, and the
/// settlement timer is far too slow to keep it current on its own. The cycle
/// runs every two seconds, so this is roughly half a minute: one extra
/// `/query/balance` per 30s against the node, and never on a miner's request
/// path.
const MONEY_REFRESH_CYCLES: u64 = 15;
/// Template cycles a submitted block may go unnoticed by the chain before the
/// pool shouts. `/submit/block` validates ASYNCHRONOUSLY and answers before the
/// verdict, so the only evidence a block was refused is that the tip never
/// reaches its height. At roughly one cycle every two seconds this is about two
/// minutes, far longer than a node needs to insert a block it accepted.
const BLOCK_STALL_CYCLES: u32 = 60;

static CONNS: AtomicUsize = AtomicUsize::new(0);
static NOTICE_WAITERS: AtomicUsize = AtomicUsize::new(0);
static PER_IP: LazyLock<Mutex<HashMap<IpAddr, u32>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// Last printed reason (and when) for mining without the node's transactions.
static TX_WARN: LazyLock<Mutex<Option<(String, Instant)>>> = LazyLock::new(|| Mutex::new(None));
/// Submitted blocks the chain has not reached yet, and for how many cycles.
static BLOCK_STALL: LazyLock<Mutex<HashMap<(u64, [u8; 32]), u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Lock the pool, recovering from a poisoned mutex instead of cascading panics.
/// With panic=unwind a handler that panics under the lock poisons it; recovering
/// keeps the pool serving instead of turning one fault into permanent death.
fn plock(m: &Mutex<Pool>) -> MutexGuard<'_, Pool> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn per_ip_lock() -> MutexGuard<'static, HashMap<IpAddr, u32>> {
    PER_IP.lock().unwrap_or_else(|e| e.into_inner())
}

/// Releases a connection's global + per-IP slot on scope exit — including on an
/// unwind — so a panicking handler can never leak a slot and wedge the listener.
struct ConnGuard {
    ip: Option<IpAddr>,
}
impl Drop for ConnGuard {
    fn drop(&mut self) {
        CONNS.fetch_sub(1, Relaxed);
        if let Some(ip) = self.ip {
            let mut m = per_ip_lock();
            if let Some(c) = m.get_mut(&ip) {
                *c -= 1;
                if *c == 0 {
                    m.remove(&ip);
                }
            }
        }
    }
}

/// Releases a long-poll waiter slot on scope exit.
struct NoticeGuard;
impl Drop for NoticeGuard {
    fn drop(&mut self) {
        NOTICE_WAITERS.fetch_sub(1, Relaxed);
    }
}

/// A serialized accounting snapshot on its way to disk. Building it needs the
/// pool lock; WRITING it must not hold it. Every served endpoint takes the same
/// mutex, so a create/rename/fsync on a slow, full or networked disk would
/// otherwise freeze work distribution and share acceptance for every miner for
/// as long as the disk takes.
struct StateShot {
    seq: u64,
    path: String,
    body: Vec<u8>,
    durable: bool,
}

/// Orders state writes and remembers the newest snapshot that reached disk, so a
/// writer that lost a race can never overwrite a fresher snapshot with a stale
/// one. Held only around the write itself, never together with the pool lock.
static PERSIST: LazyLock<Mutex<u64>> = LazyLock::new(|| Mutex::new(0));

/// Write a snapshot to disk, OFF the pool lock. The file is written atomically
/// (temp + optional fsync + rename) by `hbit_pool::atomic_write`, so a crash or
/// a full disk mid-write can never leave a truncated or corrupt file.
///
/// Returns false only if this snapshot (or a newer one) did NOT reach disk, so a
/// caller about to move money can refuse to proceed rather than pay out
/// untracked.
fn flush_state(shot: Option<StateShot>) -> bool {
    let Some(shot) = shot else {
        return true;
    };
    let mut last = PERSIST.lock().unwrap_or_else(|e| e.into_inner());
    if shot.seq <= *last {
        // A newer snapshot already landed. It was taken under the pool lock after
        // this one, so it carries everything this one carried.
        return true;
    }
    if let Err(e) = atomic_write(&shot.path, &shot.body, shot.durable) {
        eprintln!("[state] save failed ({e}); accounting NOT flushed this round");
        return false;
    }
    *last = shot.seq;
    true
}

/// A valuation of the pool's own wallet, taken off the pool lock.
///
/// `units` is what has MATURED and is not yet settled: the node's confirmed
/// balance, minus block income a reorg could still revoke, minus the fee reserve.
/// It is deliberately a snapshot with a timestamp: `/earnings` must never make a
/// node call, and a miner reading a figure has to be able to see how old it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Matured {
    units: u64,
    at: u64,
}

/// `durable` fsyncs before the rename; the frequent debounced share-save skips it
/// (a crash loses at most the last handful of shares, which is already the
/// accepted tolerance).
struct Pool {
    node: String,
    payout: String,
    state_file: String,
    client: reqwest::blocking::Client,
    params: ChainParams,
    tpl: Template,
    share_target: [u8; 32],
    /// How many powers of two EASIER than the network target a share is. The
    /// share target is derived from the live network difficulty (not an absolute
    /// value), so it scales as difficulty changes and a share represents a fixed
    /// fraction of a real block — which is what makes credit proportional to
    /// hashrate instead of to batch cadence.
    share_factor: u32,
    /// The factor the pool is REALLY serving. `share_target_hash` saturates at
    /// the all-0xff ceiling, so on a low-difficulty chain the target handed out
    /// can be far easier than `share_factor` asks for - and at the ceiling every
    /// hash is a share, which is exactly when credit stops tracking hashrate.
    /// Derived from the two targets, never assumed, and re-derived on every
    /// difficulty change.
    share_factor_achieved: u32,
    network_target: [u8; 32],
    /// Cached /query/miner/pending response for the current template, rebuilt
    /// only when the template changes so a poll never rebuilds it under the lock.
    pending_cache: String,
    workers: HashMap<String, [u8; 32]>,
    next_en: u64,
    pplns: Pplns,
    accepted: u64,
    blocks: u64,
    orphaned: u64,
    /// Solutions already credited for the current template — rejects replays.
    seen: HashSet<(u64, [u8; 32], u32)>,
    /// Blocks we submitted, awaiting confirmation that they stuck.
    submitted: Vec<(u64, [u8; 32])>,
    /// Found blocks whose coinbase is NOT yet safe to distribute:
    /// (height, our hash, reward in units of 0.1 HAC). An entry leaves only once
    /// the chain still holds OUR hash COINBASE_MATURITY_DEPTH blocks later, or
    /// immediately once the chain shows a different hash there (orphaned, so
    /// that income never lands). Its reward is held back from `distributable`
    /// until then, and it is persisted so a restart cannot forget the hold-back.
    immature: Vec<(u64, [u8; 32], u64)>,
    /// Accepted shares not yet flushed to disk (debounces state writes).
    unsaved: u32,
    /// Monotonic snapshot counter: the state writer uses it to drop a snapshot
    /// that a fresher one has already overtaken.
    state_seq: u64,
    /// Hashes of payout transactions that have not yet confirmed. While ANY is
    /// still in the mempool we must not settle again (double spend). Persisted so
    /// a restart mid-settlement does not re-pay. Robust to a lost submit ACK and
    /// to the wallet also earning coinbase income.
    settle_pending_txs: Vec<String>,
    /// The exact per-recipient rows of every payout in `settle_pending_txs` this
    /// pool submitted itself. A hash alone can only say that SOME payout is in
    /// flight; these are what let a miner be told what is in flight FOR IT, and
    /// what let a confirmed payout be credited to the right people.
    payout_records: Vec<PayoutRecord>,
    /// Total in-flight units across `payout_records`. Derived, kept beside them
    /// so the PENDING estimate can subtract it in O(1): the node's CONFIRMED
    /// balance still contains money that is already in a submitted payout, so
    /// without this subtraction the same units would be reported as pending AND
    /// as in flight.
    inflight_units: u64,
    /// What the pool has actually PAID each worker, folded in only when the node
    /// reports a payout buried. Persisted, and only ever grows.
    paid: PaidLedger,
    /// The pool's last successful valuation of its own wallet: what has matured
    /// and is not yet settled. `None` means the pool has never been able to value
    /// its balance, and PENDING must then be reported as unknown - never as zero.
    matured: Option<Matured>,
    /// Did the most recent valuation attempt succeed? A stale figure is still
    /// worth reporting, but a miner has to be told it is stale.
    matured_current: bool,
    /// How often the settlement timer runs, so `/terms` states the interval this
    /// process is actually using rather than a documented default.
    settle_secs: u64,
    /// Consecutive settlement cycles skipped because a payout was still waiting
    /// in the mempool. A payout that never confirms silently freezes every later
    /// payout, so this drives an escalating warning instead of silence. Purely a
    /// diagnostic, so it is deliberately not persisted.
    settle_stalls: u64,
    /// Consecutive above-target submissions per worker, cleared by that worker's
    /// next accepted share. The pool never trusts a worker's own header: it
    /// rebuilds the header from (height, coinbase_nonce, block_nonce) and hashes
    /// that, so a worker computing a different merkle root is REJECTED rather
    /// than credited - it cannot steal, but it also cannot earn, and counting
    /// those rejects in silence is how a worker mines for nothing all day.
    bad_streak: HashMap<String, u64>,
}

impl Pool {
    /// Snapshot accounting for the disk at most every 16 shares. Block events
    /// snapshot directly, so a crash loses at worst a handful of shares. The
    /// caller must RELEASE the pool lock before handing the result to
    /// `flush_state`: this is the share hot path, and every other request is
    /// serialized behind this same mutex.
    fn note_share_saved(&mut self) -> Option<StateShot> {
        self.unsaved += 1;
        if self.unsaved < 16 {
            return None;
        }
        self.unsaved = 0;
        // Non-durable: high frequency, and losing <=16 shares on a crash is
        // already the accepted tolerance. Block/settle events fsync.
        self.state_shot(false)
    }

    /// Derive the share target from the CURRENT network difficulty, so it tracks
    /// difficulty changes instead of being a fixed absolute threshold, and
    /// re-derive the factor it ACTUALLY achieves: the derivation saturates, and a
    /// difficulty fall can quietly turn a 2^24 share into "every hash counts".
    fn recompute_share_target(&mut self) {
        self.share_target = pool_core::share_target_hash(self.tpl.difficulty, self.share_factor);
        self.share_factor_achieved =
            pool_core::achieved_share_factor(&self.network_target, &self.share_target);
    }

    /// Rebuild the derived in-flight total from the payout rows.
    fn rebuild_inflight(&mut self) {
        self.inflight_units = self
            .payout_records
            .iter()
            .map(|r| r.units())
            .fold(0u64, |a, b| a.saturating_add(b));
    }

    /// Everything `/earnings` needs about one worker, gathered in one short hold
    /// of the pool lock. No node call, no full share table, no allocation beyond
    /// the handful of in-flight rows that actually name this worker.
    fn earnings_of(&self, worker: &str) -> Earnings {
        let shares = self.pplns.count_of(worker);
        let paid = self.paid.get(worker).cloned().unwrap_or_default();
        let mut inflight = Vec::new();
        let mut inflight_units = 0u64;
        for r in &self.payout_records {
            let u = r.units_for(worker);
            if u == 0 {
                continue;
            }
            inflight_units = inflight_units.saturating_add(u);
            inflight.push(InflightRow {
                hash: r.hash.clone(),
                units: u,
                at: r.at,
                node_holds: r.node_holds,
            });
        }
        Earnings {
            // The pool knows a worker if it holds shares for it now, has paid it,
            // owes it something in flight, or has handed it work. Anything else
            // is an address this pool has never heard of, which is NOT the same
            // fact as a worker that is owed nothing.
            known: shares > 0
                || paid.units > 0
                || inflight_units > 0
                || self.workers.contains_key(worker),
            shares,
            window_shares: self.pplns.total(),
            window_size: self.pplns.window() as u64,
            paid,
            paid_since: self.paid.since,
            inflight_units,
            inflight,
            // The confirmed balance still holds money that is already inside a
            // submitted payout, so the pool-wide pending pot is what has matured
            // MINUS what is in flight. Without this the same units would be
            // reported to the same miner twice.
            pool_pending_units: self
                .matured
                .map(|m| m.units.saturating_sub(self.inflight_units)),
            matured_at: self.matured.map(|m| m.at).unwrap_or(0),
            matured_current: self.matured_current,
            // Payouts the pool is tracking but has no rows for: written by an
            // older build, or by a tool that recorded only the hash. Their units
            // are still inside the confirmed balance, so the subtraction above
            // cannot be complete and PENDING would be overstated. Counted here so
            // it can be reported as unknown rather than as a number that is too
            // high, which is the one direction that would promise money.
            unattributed_payouts: self
                .settle_pending_txs
                .iter()
                .filter(|h| !self.payout_records.iter().any(|r| &&r.hash == h))
                .count(),
        }
    }

    /// Rebuild the cached standard-API pending response for the current template.
    fn rebuild_pending_cache(&mut self) {
        self.pending_cache = pending_cache_json(&self.tpl, &self.share_target);
    }

    /// Record one above-target submission. Returns the streak length when the
    /// pool should shout about it.
    fn note_bad_share(&mut self, worker: &str) -> Option<u64> {
        bump_bad_streak(&mut self.bad_streak, worker)
    }

    /// A share was accepted: this worker and the pool agree on the header again.
    fn note_good_share(&mut self, worker: &str) {
        self.bad_streak.remove(worker);
    }

    /// Stable per-worker extranonce -> private search space (coinbase miner_nonce).
    /// The /work protocol is anonymous, so cap the map: past the cap, hand out a
    /// deterministic extranonce derived from the name instead of storing it, so a
    /// flood of unique names cannot grow memory without bound.
    fn extranonce_for(&mut self, worker: &str) -> [u8; 32] {
        if let Some(en) = self.workers.get(worker) {
            return *en;
        }
        if self.workers.len() >= 100_000 {
            let mut en = [0u8; 32];
            en[0..8].copy_from_slice(&(worker.len() as u64).to_be_bytes());
            for (i, b) in worker.bytes().enumerate() {
                en[8 + (i % 24)] ^= b;
            }
            return en;
        }
        self.next_en += 1;
        let mut en = [0u8; 32];
        en[24..32].copy_from_slice(&self.next_en.to_be_bytes());
        self.workers.insert(worker.to_string(), en);
        en
    }

    /// Serialize the accounting for `flush_state` to write once the pool lock is
    /// released. `durable` fsyncs before the rename (block-found and settlement
    /// events); the debounced share-save does not.
    fn state_shot(&mut self, durable: bool) -> Option<StateShot> {
        if self.state_file.is_empty() {
            return None;
        }
        let body = json!({
            "window": PPLNS_WINDOW,
            "order": self.pplns.snapshot(),
            "accepted": self.accepted,
            "blocks": self.blocks,
            "orphaned": self.orphaned,
            "settle_pending_txs": self.settle_pending_txs,
            // The per-recipient rows behind those hashes, and the confirmed
            // totals they turn into. Both must move in the SAME snapshot: a
            // payout leaves the in-flight rows at the instant it enters the paid
            // totals, and a crash between the two would either lose a payment or
            // report it twice.
            "payouts_inflight": self.payout_records.iter().map(|r| r.to_json())
                .collect::<Vec<_>>(),
            "paid": self.paid.to_json(),
            // Blocks still awaiting confirmation. Without these a restart in the
            // window between finding a block and burying it drops it from the
            // confirm/orphan reconciliation for good, so a later reorg of one of
            // OUR blocks is never detected and the operator's stats drift.
            "submitted": self.submitted.iter().map(|(h, hash)| json!({
                "height": h,
                "hash": hex::encode(hash),
            })).collect::<Vec<_>>(),
            "immature": self.immature.iter().map(|(h, hash, u)| json!({
                "height": h,
                "hash": hex::encode(hash),
                "units": u,
            })).collect::<Vec<_>>(),
        });
        self.state_seq += 1;
        Some(StateShot {
            seq: self.state_seq,
            path: self.state_file.clone(),
            body: body.to_string().into_bytes(),
            durable,
        })
    }

    fn load_state(&mut self) {
        let Ok(txt) = std::fs::read_to_string(&self.state_file) else {
            return;
        };
        let j: serde_json::Value = match serde_json::from_str(&txt) {
            Ok(j) => j,
            Err(e) => {
                // Never silently wipe accounting: preserve the corrupt file and
                // start fresh only after loudly flagging it for the operator.
                let bak = format!("{}.corrupt.{}", self.state_file, std::process::id());
                let _ = std::fs::rename(&self.state_file, &bak);
                eprintln!(
                    "[state] file corrupt ({e}); preserved as {bak}, starting with empty accounting"
                );
                return;
            }
        };
        let order: Vec<String> = j
            .get("order")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        self.pplns = Pplns::restore(PPLNS_WINDOW, order);
        self.accepted = j.get("accepted").and_then(|v| v.as_u64()).unwrap_or(0);
        self.blocks = j.get("blocks").and_then(|v| v.as_u64()).unwrap_or(0);
        self.orphaned = j.get("orphaned").and_then(|v| v.as_u64()).unwrap_or(0);
        self.settle_pending_txs = j
            .get("settle_pending_txs")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        // Blocks awaiting confirmation must survive a restart too, or one of OUR
        // blocks being orphaned goes unnoticed and blocks_confirmed permanently
        // over-counts against what the chain actually holds.
        self.submitted = j
            .get("submitted")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        let h = x.get("height").and_then(|v| v.as_u64())?;
                        let hash = hash32(x.get("hash").and_then(|v| v.as_str())?)?;
                        Some((h, hash))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The hold-back must survive a restart: forgetting it would let the very
        // next settle cycle distribute income a reorg can still revoke.
        self.immature = j
            .get("immature")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        let h = x.get("height").and_then(|v| v.as_u64())?;
                        let u = x.get("units").and_then(|v| v.as_u64())?;
                        let hash = hash32(x.get("hash").and_then(|v| v.as_str())?)?;
                        Some((h, hash, u))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The per-worker settlement ledger. Losing this would not lose anyone's
        // money - the chain holds that - but it WOULD reset every miner's "total
        // paid" to zero and make the pool report a number that quietly means
        // something else, so it is restored with everything else and its start
        // time travels with it.
        self.payout_records = parse_payout_records(&j);
        self.paid = parse_paid_ledger(&j);
        self.rebuild_inflight();
        println!(
            "restored accounting: {} shares in window, {} blocks, {} orphaned, \
             {} block(s) awaiting confirmation, {} payout(s) pending, \
             {} block(s) of income not yet matured, \
             {} worker(s) with a paid history",
            self.pplns.total(),
            self.blocks,
            self.orphaned,
            self.submitted.len(),
            self.settle_pending_txs.len(),
            self.immature.len(),
            self.paid.workers()
        );
    }
}

/// Count one above-target submission from `worker`, returning the streak length
/// when it is time to shout.
///
/// Shouts at `BAD_STREAK_WARN` and at every further multiple, so a worker that
/// never recovers keeps saying so without turning the log into noise. The map is
/// bounded: a flood of invented worker ids must not grow memory, and the
/// diagnostic exists for real miners.
fn bump_bad_streak(streaks: &mut HashMap<String, u64>, worker: &str) -> Option<u64> {
    if !streaks.contains_key(worker) && streaks.len() >= BAD_STREAK_WORKERS {
        return None;
    }
    let n = streaks.entry(worker.to_string()).or_insert(0);
    *n += 1;
    (*n >= BAD_STREAK_WARN && *n % BAD_STREAK_WARN == 0).then_some(*n)
}

/// The standard-API `/query/miner/pending` body for a template.
///
/// `mkrl_modify_list` is NOT optional once the template carries the node's
/// transactions. A worker rebuilds the merkle root from its OWN coinbase nonce
/// folded through this list; serving an empty list while the block holds
/// transactions makes the worker hash a header the pool never reconstructs, so
/// every share it finds is rejected. The pool used to hard-code `[]` here, which
/// was safe only because its blocks were always coinbase-only.
fn pending_cache_json(tpl: &Template, share_target: &[u8; 32]) -> String {
    let cb = coinbase_with_extranonce(tpl, &[0u8; 32]);
    let intro = intro_bytes(tpl, &cb, 0);
    let mkrl: Vec<String> = tpl
        .txs
        .mrklrts
        .iter()
        .map(|h| hex::encode(h.serialize()))
        .collect();
    json!({
        "ret": 0,
        "height": tpl.height,
        "block_intro": hex::encode(intro),
        "target_hash": hex::encode(share_target),
        "coinbase_body": coinbase_body_hex(&cb),
        "mkrl_modify_list": mkrl,
    })
    .to_string()
}

/// Refuse a share size that would make the equal-weight PPLNS window unfair.
///
/// Every accepted share is credited with weight 1, and the network difficulty in
/// force when it was mined is not recorded. That is exact only while the whole
/// window is far SHORTER than one block interval, because difficulty moves only
/// at a block boundary: with 2^factor shares to a block, PPLNS_WINDOW shares
/// span PPLNS_WINDOW / 2^factor of a block - 0.02% at the default 24, still only
/// 1.6% at 18. Go lower and a difficulty change lands inside a live window, so
/// real payout money is split by share counts that stand for different amounts
/// of work. The upper bound keeps a share from being so easy that a whole GPU
/// batch always beats it (credit would then track batch cadence, not hashrate)
/// and that the share target degenerates into the all-0xff ceiling.
fn check_share_factor(factor: u32) -> Result<(), String> {
    if !(MIN_SHARE_FACTOR..=MAX_SHARE_FACTOR).contains(&factor) {
        return Err(format!(
            "share_bits must be between {MIN_SHARE_FACTOR} and {MAX_SHARE_FACTOR} (got {factor}).\n\
             A share is 2^share_bits easier than a network block; below {MIN_SHARE_FACTOR} the \
             {PPLNS_WINDOW}-share payout window covers enough of a block interval that a \
             difficulty change inside it would misallocate real payouts."
        ));
    }
    Ok(())
}

/// Refuse a share target the live network difficulty cannot support.
///
/// `check_share_factor` only ever looked at the number the operator typed. The
/// target actually served is `network_target * 2^factor`, computed by
/// `share_target_hash`, which SATURATES at the all-0xff ceiling and says nothing
/// when it does. On a low-difficulty chain - a fresh testnet sits at
/// `LOWEST_DIFFICULTY`, whose target has no leading zero bits at all - every hash
/// then beats the share target. A worker is no longer credited for work: it is
/// credited for how often it can complete an HTTP round trip, and one worker in a
/// tight loop takes the whole payout window from everyone else. That is not a
/// tuning wrinkle, it is the pool paying the wrong people, so it is refused.
///
/// The bound is the SAME `MIN_SHARE_FACTOR` the operator's `share_bits` must
/// clear, applied to the factor the pool really achieves.
fn check_share_target(factor: u32, achieved: u32, difficulty: u32) -> Result<(), String> {
    if achieved >= MIN_SHARE_FACTOR {
        return Ok(());
    }
    Err(format!(
        "the network difficulty in force ({difficulty}) is too low to serve a fair share.\n\
         share_bits={factor} asks for a share 2^{factor} easier than a block, but the derived \
         share target saturates and what workers actually get is 2^{achieved} \
         (minimum {MIN_SHARE_FACTOR}).\n\
         At that point a share costs almost no work, so PPLNS credit tracks how fast a worker \
         can submit rather than how much it hashes, and one worker can take the whole \
         {PPLNS_WINDOW}-share payout window from everyone else. This pool will not distribute \
         real money on that basis.\n\
         Point it at a chain whose difficulty has risen, or wait for this one to adjust."
    ))
}

/// Plan a settlement on the pool's ADVERTISED terms.
///
/// The only place the split is parameterised, and `/terms` reports the very same
/// constants, so the fee and the minimum payout a miner is told about are the
/// fee and the minimum payout the pool applies.
fn plan_settlement(distributable: u64, payable: &[(String, u64)]) -> Vec<(String, u64)> {
    split_payout(distributable, POOL_FEE_UNITS, PAYOUT_DUST_UNITS, payable)
}

/// A money figure, in the chain's own terms.
///
/// `units` is a whole number of 0.1 HAC (the granularity every payout this pool
/// makes is planned in) and `amount` is that same number as the chain's `Amount`
/// in its canonical `mantissa:unit` form - the identical string the node speaks
/// and the transaction carries. No float, no hand-rolled decimal.
fn money(units: u64) -> serde_json::Value {
    json!({
        "units": units,
        "amount": payout_amount(units).to_fin_string(),
        "unit": PAYOUT_UNIT,
    })
}

/// What `/earnings` can say about a worker id before it looks anything up.
///
/// Three different facts, and a miner must never be shown one when another is
/// true: a string that is not an address at all, an address this pool could never
/// pay (so it never credits shares to it), and an address that is fine - after
/// which "the pool has never heard of it" and "it is owed nothing" are two more
/// answers again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerId {
    Payable,
    Unpayable,
    NotAnAddress,
}

fn classify_worker_id(s: &str) -> WorkerId {
    if Address::from_readable(s).is_err() {
        return WorkerId::NotAnAddress;
    }
    if is_payout_address(s) {
        WorkerId::Payable
    } else {
        WorkerId::Unpayable
    }
}

/// One payout that is submitted but not yet confirmed, as it concerns one worker.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InflightRow {
    hash: String,
    units: u64,
    at: u64,
    node_holds: bool,
}

/// Everything `/earnings` says about one worker, gathered under the pool lock and
/// rendered with it released.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Earnings {
    /// Has this pool ever heard of this address? A worker it has never seen is a
    /// different fact from a worker that is owed nothing, and reporting the first
    /// as the second tells a miner its pool is tracking work that it is not.
    known: bool,
    shares: u64,
    window_shares: u64,
    window_size: u64,
    paid: hbit_pool::PaidRow,
    paid_since: u64,
    inflight_units: u64,
    inflight: Vec<InflightRow>,
    /// Pool-wide matured-and-unsettled pot, already net of everything in flight.
    /// `None` means the pool cannot value its own wallet right now.
    pool_pending_units: Option<u64>,
    matured_at: u64,
    matured_current: bool,
    /// Tracked payouts whose per-recipient rows this pool does not have. While
    /// there is even one, the pending pot cannot be computed honestly.
    unattributed_payouts: usize,
}

/// One worker's share of the pool-wide pending pot.
///
/// The same floor split `split_payout` applies, for one worker: a settlement
/// hands out `pot * mine / total` rounded down, and anything under the minimum
/// payout is not paid at all. Deliberately does NOT add the largest-remainder
/// unit `split_payout` may award, so this figure is never higher than what a
/// settlement at this instant would actually pay.
///
/// Every share in the window belongs to a payable address - `handle_submission`
/// refuses any other - so the window total IS the payable total the split runs
/// over.
fn worker_pending_units(pot: u64, mine: u64, window_total: u64) -> u64 {
    if pot == 0 || mine == 0 || window_total == 0 {
        return 0;
    }
    let share = (pot as u128 * mine as u128 / window_total as u128) as u64;
    // Below the advertised minimum a settlement pays nothing at all, so promising
    // a fraction of it would be promising money that never moves.
    if share < PAYOUT_DUST_UNITS { 0 } else { share }
}

/// The `/earnings` body for one worker.
fn earnings_json(worker: &str, e: &Earnings, now: u64) -> serde_json::Value {
    if !e.known {
        // No numbers at all. A zero here would read as "your pool owes you
        // nothing", when the truth is that it has never seen this address.
        return json!({
            "ok": true,
            "kind": "unknown_worker",
            "worker": worker,
            "known": false,
            "err": "this pool has no record of that address: it holds no shares for it, has \
                    never paid it, and has no payout in flight for it",
        });
    }
    let pending = match e.pool_pending_units.filter(|_| e.unattributed_payouts == 0) {
        Some(pot) => {
            let units = worker_pending_units(pot, e.shares, e.window_shares);
            json!({
                "known": true,
                "estimate": true,
                "units": units,
                "amount": payout_amount(units).to_fin_string(),
                "unit": PAYOUT_UNIT,
                "as_of_unix": e.matured_at,
                "as_of_age_secs": now.saturating_sub(e.matured_at),
                "current": e.matured_current,
                "note": "an ESTIMATE, not a promise: it is this worker's share of what the pool \
                         has matured and not yet settled, and it moves every time any worker's \
                         share enters or leaves the payout window. Shares that leave the window \
                         before a settlement earn nothing.",
            })
        }
        // Rule of the house: never show a number the pool cannot stand behind.
        None if e.unattributed_payouts > 0 => json!({
            "known": false,
            "reason": format!(
                "{} payout(s) this pool is tracking have no recipient detail behind them, so it \
                 cannot separate what is still owed from what is already in flight and would \
                 overstate this figure. It resolves itself as those payouts confirm. This is not \
                 a zero.",
                e.unattributed_payouts
            ),
        }),
        None => json!({
            "known": false,
            "reason": "the pool has not been able to value its own wallet balance, so it cannot \
                       say what is owed. This is not a zero.",
        }),
    };
    json!({
        "ok": true,
        "kind": "worker",
        "worker": worker,
        "known": true,
        "scheme": "PPLNS",
        "shares_in_window": e.shares,
        "window_shares": e.window_shares,
        "window_size": e.window_size,
        // Confirmed on chain. Only ever grows, and only when the node reports the
        // paying transaction buried.
        "paid": money(e.paid.units),
        "paid_since_unix": e.paid_since,
        "last_payout": if e.paid.last_units > 0 {
            json!({
                "units": e.paid.last_units,
                "amount": payout_amount(e.paid.last_units).to_fin_string(),
                "unit": PAYOUT_UNIT,
                "tx": e.paid.last_hash,
                "at_unix": e.paid.last_at,
            })
        } else {
            // Never paid by this ledger. An explicit null, not a zero amount.
            serde_json::Value::Null
        },
        // Submitted, not confirmed: neither paid nor pending.
        "in_flight": {
            "units": e.inflight_units,
            "amount": payout_amount(e.inflight_units).to_fin_string(),
            "unit": PAYOUT_UNIT,
            "txs": e.inflight.iter().map(|r| json!({
                "tx": r.hash,
                "units": r.units,
                "amount": payout_amount(r.units).to_fin_string(),
                "submitted_unix": r.at,
                // false = submitted, but the node's verdict could not be read.
                // The pool keeps tracking it and claims nothing about it.
                "node_holds": r.node_holds,
            })).collect::<Vec<_>>(),
            "note": "submitted to the node and NOT yet confirmed on chain. Nothing here is paid \
                     yet, and none of it is counted in pending.",
        },
        "pending": pending,
        "buckets": "paid, in_flight and pending are disjoint: a unit is in exactly one of them.",
    })
}

/// The pool's terms, read out of the code that enforces them.
///
/// Every number here is the constant or the live value some other part of this
/// pool acts on, so what is advertised cannot drift from what is done. Nothing in
/// it is typed twice.
#[allow(clippy::too_many_arguments)]
fn terms_json(
    window_size: u64,
    share_factor: u32,
    share_factor_achieved: u32,
    difficulty: u32,
    settle_secs: u64,
    coinbase_maturity: u64,
    payout_confirm_depth: u64,
) -> serde_json::Value {
    json!({
        "ok": true,
        "scheme": "PPLNS",
        "scheme_full": "Pay Per Last N Shares",
        "scheme_note": "Every settlement is split over the last N accepted shares the pool holds \
                        at that moment, whoever found them and whenever they were found. There \
                        are no rounds: this is NOT PROP, which would pay each block's shares in \
                        proportion to that block's round.",
        "window_shares": window_size,
        "share_factor": share_factor,
        "share_factor_achieved": share_factor_achieved,
        "share_factor_note": "a share is 2^share_factor_achieved times easier to find than a \
                              network block at the current difficulty. The pool refuses to serve \
                              work when the achieved factor falls below its minimum, because a \
                              share that costs almost nothing makes credit track submission rate \
                              instead of hashrate.",
        "network_difficulty": difficulty,
        "fee": money(POOL_FEE_UNITS),
        "fee_note": "this pool takes no fee: the whole matured balance is split over the share \
                     window.",
        "minimum_payout": money(PAYOUT_DUST_UNITS),
        "minimum_payout_note": "a worker whose share of a settlement rounds below this is paid \
                                nothing that cycle. That money is not taken from anyone: it stays \
                                in the pool wallet and is part of the next cycle's balance.",
        "fee_reserve": money(SETTLE_RESERVE_UNITS),
        "fee_reserve_note": "kept in the pool wallet so it can always fund the network fee on a \
                             settlement transaction. Not a fee, and not skimmed: a later cycle \
                             distributes whatever of it is no longer needed.",
        "network_fee_per_settlement_tx": chunk_tx_fee().to_fin_string(),
        "recipients_per_settlement_tx": PAYOUT_CHUNK,
        "settle_interval_secs": settle_secs,
        "coinbase_maturity_blocks": coinbase_maturity,
        "coinbase_maturity_note": "income from a block this pool finds is held back until the \
                                   chain has buried it this deep. Paying it out earlier and then \
                                   losing the block to a reorg would spend money the chain no \
                                   longer holds.",
        "payout_confirm_blocks": payout_confirm_depth,
        "payout_confirm_note": "a payout counts as PAID only once the node reports the paying \
                                transaction this many blocks deep. Until then it is in flight.",
        "share_eviction_note": "a share is credited only while it is among the last \
                                window_shares the pool accepted. Once newer shares push it out it \
                                earns nothing, and nothing is carried forward for it.",
        "payout_address_required": true,
        "payout_address_note": "the worker id IS the payout address. The pool refuses a share \
                                from any id it could not pay, so nobody mines for an id that \
                                would be dropped at settlement.",
        "amount_unit": PAYOUT_UNIT,
        "amount_note": "`units` are whole 0.1 HAC and `amount` is the same figure as the chain's \
                        own mantissa:unit amount. The pool never reports money as a decimal it \
                        rounded itself.",
    })
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let node = a
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:8088".to_string());
    let node = node.trim_end_matches('/').to_string();
    let wallet_file = a
        .get(2)
        .cloned()
        .unwrap_or_else(|| "pool-wallet.key".to_string());
    let listen = a
        .get(3)
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:9777".to_string());
    // How many powers of two easier than a network block a share is. Tune to the
    // miner population and GPU batch size: too small and small miners rarely find
    // a share; too large and a whole GPU batch's best hash always beats it, so
    // credit tracks batch cadence rather than hashrate. ~24 suits GPU batches.
    let share_factor: u32 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(24);
    if let Err(e) = check_share_factor(share_factor) {
        eprintln!("{e}");
        std::process::exit(2);
    }
    // chain is REQUIRED: a mainnet pool run with testnet difficulty (or vice
    // versa) computes the wrong target and every block/share is rejected. Refuse
    // to guess.
    let Some(chain) = a.get(5).cloned() else {
        eprintln!(
            "usage: hbit-pool-server <node> <wallet_file> <listen> <share_bits> <chain> [settle_secs]\n\
             `chain` is required: `mainnet`, `testnet`, or \
             `testnet:<difficulty_adjust_blocks>:<each_block_target_time>`."
        );
        std::process::exit(2);
    };
    // A testnet node takes its difficulty window and block time from its OWN
    // config file, so accept them spelled out rather than assuming a pair that
    // would make the node reject every block this pool mines.
    let Some(params) = ChainParams::parse(&chain) else {
        eprintln!(
            "chain must be `mainnet`, `testnet`, or \
             `testnet:<difficulty_adjust_blocks>:<each_block_target_time>` (got `{chain}`)"
        );
        std::process::exit(2);
    };
    let settle_secs: u64 = a.get(6).and_then(|s| s.parse().ok()).unwrap_or(300);

    // Name the PRODUCT, not the executable. An operator reading a terminal or
    // pasting a log into a support thread has to be able to say what is running,
    // and a file name does not tell them: this is the HBIT pool.
    println!("== HBIT pool server v{} ==", env!("CARGO_PKG_VERSION"));
    println!("node    = {node}");
    // Exactly one process may settle a wallet, enforced by the OS for as long as
    // this one lives. `hbit-pool-payout` takes the SAME lock, so it can never
    // pay out of a wallet this server is already settling: both read the CONFIRMED
    // balance (a payout waiting in the mempool does not reduce it), so each would
    // see the full balance and pay the same PPLNS window a second time.
    let _settle_lock = match acquire_settle_lock(&wallet_file) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "another hbit-pool-server or hbit-pool-payout already holds {wallet_file} ({e}).\n\
                 Only one process may settle a wallet - stop the other one first."
            );
            std::process::exit(2);
        }
    };
    let wallet = load_or_create_wallet(&wallet_file);
    let payout = wallet.readable().to_string();

    let client = http_client();
    // Prove the difficulty rule in force here reproduces the node's OWN tip
    // before serving a single piece of work. Otherwise a chain label that does
    // not match the node's config makes every block the pool finds rejected, and
    // nothing says so: the pool just mines dead work indefinitely.
    if let Err(e) = verify_chain_params(&client, &node, &params) {
        eprintln!("REFUSING to start: {e}");
        std::process::exit(2);
    }
    let (tpl, txs_note) = fetch_pool_template(&client, &node, &payout, &params, None)
        .expect("could not fetch an initial template — is the node running and synced?");
    let network_target = tpl.target;
    // The factor the operator asked for is not necessarily the factor workers
    // get: the derivation saturates. Refuse to serve, and to distribute real
    // money, on a share nobody had to work for.
    let share_target = pool_core::share_target_hash(tpl.difficulty, share_factor);
    let share_factor_achieved = pool_core::achieved_share_factor(&network_target, &share_target);
    if let Err(e) = check_share_target(share_factor, share_factor_achieved, tpl.difficulty) {
        eprintln!("REFUSING to start: {e}");
        std::process::exit(2);
    }

    println!("listen  = {listen}");
    println!("chain   = {chain} (ASERT at height {})", params.asert_height);
    println!("share   = 2^{share_factor_achieved} easier than a network block");
    if share_factor_achieved < share_factor {
        // Above the refusal bound, so shares are still worth something, but not
        // what was asked for: say the real number rather than let the operator
        // believe a figure the chain will not support.
        println!(
            "          (share_bits={share_factor} was asked for; the difficulty in force caps it \
             at 2^{share_factor_achieved})"
        );
    }
    println!("settle  = every {settle_secs}s");
    println!(
        "height  = {} (template, difficulty {}, {} packed tx(s) from the node)",
        tpl.height,
        tpl.difficulty,
        tpl.txs.bodies.len()
    );
    // Say at startup, not only on the next cycle, whether this pool will mine the
    // node's transactions. Mining empty blocks is the failure that leaves the
    // pool's own payouts stuck in the mempool forever.
    report_packed_txs(txs_note.as_deref());

    let mut pool = Pool {
        node: node.clone(),
        payout,
        state_file: pool_state_path(&wallet_file),
        client,
        params,
        share_target,
        tpl,
        share_factor,
        share_factor_achieved,
        network_target,
        pending_cache: String::new(),
        workers: HashMap::new(),
        next_en: 0,
        pplns: Pplns::new(PPLNS_WINDOW),
        accepted: 0,
        blocks: 0,
        orphaned: 0,
        seen: HashSet::new(),
        submitted: Vec::new(),
        immature: Vec::new(),
        unsaved: 0,
        state_seq: 0,
        settle_pending_txs: Vec::new(),
        payout_records: Vec::new(),
        inflight_units: 0,
        // A ledger with no start time would report "paid since the beginning of
        // time". `load_state` replaces this with the stored one if there is one.
        paid: PaidLedger::started(curtimes()),
        matured: None,
        matured_current: false,
        settle_secs,
        settle_stalls: 0,
        bad_streak: HashMap::new(),
    };
    pool.load_state();
    if pool.paid.since == 0 {
        // A state file written before the ledger existed: start counting now
        // rather than claim a total that reaches back further than it does.
        pool.paid.since = curtimes();
    }
    pool.rebuild_pending_cache();
    let pool = Arc::new(Mutex::new(pool));

    // Background: keep the template current with the chain tip and confirm our
    // submitted blocks. All node HTTP happens OFF the pool lock, so miners are
    // never stalled by it. This also advances work when the NETWORK finds a
    // block, not only when we do. The whole loop body is panic-isolated so a
    // single transient fault can never kill this long-lived thread.
    {
        let pool = pool.clone();
        let (client, node, payout, params) = {
            let p = plock(&pool);
            (
                p.client.clone(),
                p.node.clone(),
                p.payout.clone(),
                p.params.clone(),
            )
        };
        std::thread::spawn(move || {
            let mut tick: u64 = 0;
            loop {
                // The wallet valuation behind every miner's PENDING figure is
                // refreshed here, on a slow multiple of the template cycle: it is
                // one extra node call, and it must never happen on a request.
                let money = tick % MONEY_REFRESH_CYCLES == 0;
                tick = tick.wrapping_add(1);
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    template_cycle(&pool, &client, &node, &payout, &params, money);
                }));
                if let Err(e) = r {
                    eprintln!("[template] cycle panicked, continuing: {e:?}");
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
    }

    // Automatic settlement on a timer.
    {
        let p = pool.clone();
        let wf = wallet_file.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(settle_secs));
                // One bad settle cycle (poisoned lock, wallet issue, future
                // refactor) must never permanently kill payouts.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    settle_once(&p, &wf)
                }));
                if let Err(e) = r {
                    eprintln!("[settle] cycle panicked, continuing: {e:?}");
                }
            }
        });
    }

    let listener = TcpListener::bind(&listen).expect("bind");
    println!("listening...\n");
    for stream in listener.incoming() {
        let s = match stream {
            Ok(s) => s,
            // A single accept() error (e.g. EMFILE under load) must not tear down
            // the whole listener — log and keep serving.
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };
        if CONNS.load(Relaxed) >= MAX_CONNS {
            continue; // drop: s closes as it goes out of scope
        }
        let ip = s.peer_addr().ok().map(|a| a.ip());
        // Per-IP admission: one source cannot hold more than MAX_PER_IP slots,
        // so a single host cannot pin every connection with long-polls.
        if let Some(ip) = ip {
            let mut m = per_ip_lock();
            let c = m.entry(ip).or_insert(0);
            if *c >= MAX_PER_IP {
                continue; // drop this connection from a noisy IP
            }
            *c += 1;
        }
        CONNS.fetch_add(1, Relaxed);
        let guard = ConnGuard { ip };
        let p = pool.clone();
        // NEVER `thread::spawn` here: it is `Builder::spawn(..).expect(..)`, so it
        // PANICS when the OS refuses a new thread (RLIMIT_NPROC, exhausted address
        // space), and that panic unwinds straight out of main and kills the whole
        // pool. Treat a spawn failure as backpressure instead: dropping the
        // closure releases the ConnGuard (global + per-IP slot) and closes `s`.
        if let Err(e) = std::thread::Builder::new()
            .stack_size(HANDLER_STACK_BYTES)
            .spawn(move || {
                let _g = guard; // releases the slot on return AND on unwind
                handle(s, p);
            })
        {
            eprintln!("[accept] thread spawn failed, dropping connection: {e}");
        }
    }
}

/// One iteration of the background template/confirmation loop (panic-isolated by
/// the caller). Refreshes the template on a height change and confirms/orphans
/// our submitted blocks. All node HTTP is done before taking the pool lock.
fn template_cycle(
    pool: &Arc<Mutex<Pool>>,
    client: &reqwest::blocking::Client,
    node: &str,
    payout: &str,
    params: &ChainParams,
    refresh_money: bool,
) {
    // Cloning the live template is cheap (its transaction set is behind an Arc)
    // and lets an unchanged tip skip re-downloading a whole block of bodies.
    let (current, pending, immature) = {
        let p = plock(pool);
        (p.tpl.clone(), p.submitted.clone(), p.immature.clone())
    };
    let fresh = fetch_pool_template(client, node, payout, params, Some(&current));
    if let Some((_, why)) = fresh.as_ref() {
        report_packed_txs(why.as_deref());
    }
    let fresh = fresh.map(|(t, _)| t);
    let tip = fresh.as_ref().map(|t| t.height.saturating_sub(1));
    // A block the node refused leaves no other trace than the tip never reaching
    // its height, so watch for exactly that and say it out loud.
    let stalled = {
        let mut st = BLOCK_STALL.lock().unwrap_or_else(|e| e.into_inner());
        note_block_stalls(&mut st, &pending, tip)
    };
    for (h, hx) in stalled {
        eprintln!(
            "[block] the chain has STILL not reached height {h}, long after we submitted our \
             block {} there. /submit/block validates asynchronously and answers before the \
             verdict, so a refusal is silent: this block was almost certainly rejected and its \
             whole reward is lost. If the pool is packing the node's transactions, the likeliest \
             cause is that one of them was no longer valid by the time the block was submitted.",
            hex::encode(hx)
        );
    }
    // One node query per height, shared by the confirm/orphan tally and by the
    // coinbase-maturity gate below.
    let mut heights: Vec<u64> = pending
        .iter()
        .map(|(h, _)| *h)
        .chain(immature.iter().map(|(h, _, _)| *h))
        .collect();
    heights.sort_unstable();
    heights.dedup();
    let mut chain_hash: HashMap<u64, String> = HashMap::new();
    for h in heights {
        if tip.map(|t| h > t).unwrap_or(true) {
            continue; // not buried yet, or no tip this cycle
        }
        let j = get_json(client, &format!("{node}/query/block/intro?height={h}"));
        if let Some(hx) = find_str(&j, "hash") {
            chain_hash.insert(h, hx);
        }
    }
    // A block counts as confirmed only once the chain has BURIED it while still
    // holding our hash. Finalizing it the moment it merely occupies the tip (0
    // blocks stacked on top) also stops us watching it - exactly when a reorg is
    // most likely - so an orphan after that point could never be detected and
    // blocks_confirmed would over-count against the chain for good.
    let mut confirmed = Vec::new();
    let mut orphaned = Vec::new();
    for (h, ours) in &pending {
        match chain_hash.get(h) {
            Some(cur) if *cur == hex::encode(ours) => {
                if buried_deep(tip, *h) {
                    confirmed.push((*h, *ours));
                }
                // Not buried yet: keep watching it, a reorg can still flip it.
            }
            Some(cur) => {
                orphaned.push((*h, *ours));
                println!("[reorg] our block {h} orphaned (chain holds {cur})");
            }
            None => {} // node has not stored it yet; keep waiting
        }
    }
    // Coinbase maturity: a found block's reward stays held back until the chain
    // still holds OUR hash COINBASE_MATURITY_DEPTH blocks later. Releasing it any
    // earlier means the pool can pay out income that a reorg then takes back,
    // while the payout transaction that spent it stays valid on the new chain.
    let mut released: Vec<(u64, [u8; 32], u64)> = Vec::new();
    for (h, ours, u) in &immature {
        match chain_hash.get(h) {
            Some(cur) if *cur == hex::encode(ours) => {
                if buried_deep(tip, *h) {
                    released.push((*h, *ours, *u));
                }
            }
            // Orphaned: that income never lands in the balance, so there is
            // nothing left to hold back.
            Some(_) => released.push((*h, *ours, *u)),
            None => {}
        }
    }
    // Value the pool wallet OFF the lock, so `/earnings` can answer a PENDING
    // question without any node call at all. Balance FIRST and the hold-back
    // after, exactly as settlement does: a block found in between then shows up
    // in the hold-back but not yet in the balance, which errs towards reporting
    // LESS as owed.
    let money = if refresh_money {
        let bal = balance(client, node, payout);
        match balance_units(&bal) {
            Some(units) => {
                let immature_units: u64 =
                    plock(pool).immature.iter().map(|(_, _, u)| *u).sum();
                // `None` here is a KNOWN nothing (the balance is at or below the
                // reserve), which is not the same as a balance we cannot value.
                Some(distributable_units(units, immature_units, SETTLE_RESERVE_UNITS).unwrap_or(0))
            }
            // Unreadable, or implausible. NOT a zero: the last good figure is
            // kept and flagged stale rather than replaced with a made-up one.
            None => None,
        }
    } else {
        None
    };

    let mut shot = None;
    let mut degraded: Option<(u32, u32, u32)> = None;
    {
        let mut p = plock(pool);
        if refresh_money {
            p.matured_current = money.is_some();
            if let Some(units) = money {
                p.matured = Some(Matured {
                    units,
                    at: curtimes(),
                });
            }
        }
        if let Some(t) = fresh {
            // Replace the template when the tip changes: either a new height, or
            // a same-height reorg (different prev-hash). At the same height and
            // same prev-hash the timestamp/difficulty are fixed, so keeping the
            // template valid keeps every worker's in-flight share valid.
            //
            // The packed transaction set is pinned to the template for exactly
            // the same reason, and that is now load-bearing: the set determines
            // the merkle root, so swapping it mid-height would make every share
            // already found for this height - and every batch still running -
            // hash a header the pool no longer reconstructs. Every one of them
            // would be rejected. The node behaves the same way (it repacks only
            // when its own tip moves), so nothing is lost by waiting: a
            // transaction that arrives mid-height is packed into the next block.
            let changed = t.height != p.tpl.height || t.prevhash != p.tpl.prevhash;
            if changed {
                let was = p.share_factor_achieved;
                p.tpl = t;
                p.network_target = p.tpl.target;
                // Re-derive the share target from the new difficulty so shares stay
                // a fixed fraction of a block as difficulty moves.
                p.recompute_share_target();
                // A difficulty FALL can push the derived share target into
                // saturation while the pool is running, and from that moment
                // credit tracks submission rate, not hashrate. Startup refuses
                // it; here the pool is already serving miners, so say so loudly
                // and only when it changes.
                let now = p.share_factor_achieved;
                if now < MIN_SHARE_FACTOR && was >= MIN_SHARE_FACTOR {
                    degraded = Some((p.share_factor, now, p.tpl.difficulty));
                }
                let height = p.tpl.height;
                prune_seen(&mut p.seen, height);
                p.rebuild_pending_cache();
            }
        }
        p.blocks += confirmed.len() as u64;
        p.orphaned += orphaned.len() as u64;
        p.submitted
            .retain(|e| !confirmed.contains(e) && !orphaned.contains(e));
        if !released.is_empty() {
            p.immature.retain(|e| !released.contains(e));
        }
        // Block bookkeeping changed: snapshot it here, write it below with the
        // lock released.
        if !confirmed.is_empty() || !orphaned.is_empty() || !released.is_empty() {
            shot = p.state_shot(true);
        }
    }
    if let Some((asked, achieved, difficulty)) = degraded {
        eprintln!(
            "[share] the network difficulty fell to {difficulty} and the share target saturated: \
             share_bits={asked} now serves only 2^{achieved} (minimum {MIN_SHARE_FACTOR}). \
             Shares now cost almost no work, so PPLNS credit follows how fast a worker submits \
             rather than how much it hashes and the payout split is NOT trustworthy. Stop the \
             pool, or stop settling, until the difficulty recovers."
        );
    }
    flush_state(shot);
}

/// Report whether this pool is mining the node's transactions or a lone
/// coinbase, without turning the log into one line every two seconds.
///
/// A coinbase-only block is the defect this reporting exists for: it earns no
/// transaction fees, and on a chain where this pool is the only miner it means
/// the pool's OWN payout transactions can never confirm. They sit in the node's
/// mempool while block after block is mined carrying nothing.
fn report_packed_txs(why: Option<&str>) {
    let mut st = TX_WARN.lock().unwrap_or_else(|e| e.into_inner());
    match why {
        Some(why) => {
            if should_warn_now(&mut st, why, Instant::now()) {
                eprintln!(
                    "[template] mining WITHOUT the node's transactions: {why}.\n\
                     Every block this pool finds will carry nothing but its coinbase, so it \
                     collects no transaction fees, and on a chain where this pool is the only \
                     miner its OWN payouts can never confirm - they wait in the mempool while \
                     every block is mined with 0 transactions.\n\
                     Fix: the node must serve /query/miner/pending, which needs `enable = true` \
                     and a reward address under `[miner]` in the node's config."
                );
            }
        }
        None => {
            if st.take().is_some() {
                println!("[template] the node's packed transactions are available again");
            }
        }
    }
}

/// Print the packed-transaction warning now? The first occurrence and every
/// change of reason go out immediately; an unchanged reason repeats at most once
/// every `TX_WARN_REPEAT` so the operator is reminded without being buried.
fn should_warn_now(
    state: &mut Option<(String, Instant)>,
    why: &str,
    now: Instant,
) -> bool {
    match state {
        Some((prev, at)) if prev == why && now.duration_since(*at) < TX_WARN_REPEAT => false,
        _ => {
            *state = Some((why.to_string(), now));
            true
        }
    }
}

/// Count how long each submitted block has gone without the chain reaching its
/// height, and return the ones that just crossed `BLOCK_STALL_CYCLES`.
///
/// Counted ONLY while the tip is actually known, so a node that is merely
/// unreachable never trips it. An entry clears the moment the chain reaches that
/// height (accepted, or orphaned by someone else's block, which the confirm
/// tally reports separately) or the moment the pool stops tracking the block.
fn note_block_stalls(
    state: &mut HashMap<(u64, [u8; 32]), u32>,
    pending: &[(u64, [u8; 32])],
    tip: Option<u64>,
) -> Vec<(u64, [u8; 32])> {
    let Some(tip) = tip else {
        return Vec::new();
    };
    state.retain(|key, _| pending.contains(key));
    let mut shout = Vec::new();
    for key in pending {
        if key.0 <= tip {
            state.remove(key);
            continue;
        }
        let n = state.entry(*key).or_insert(0);
        *n += 1;
        // Exactly at the threshold: one loud line per lost block, not a stream.
        if *n == BLOCK_STALL_CYCLES {
            shout.push(*key);
        }
    }
    shout
}

/// Has the chain stacked COINBASE_MATURITY_DEPTH blocks on top of height `h`?
/// A `None` tip means we could not read the chain this cycle, so nothing counts
/// as buried: both callers must err towards keeping a block under observation.
fn buried_deep(tip: Option<u64>, h: u64) -> bool {
    tip.map(|t| t.saturating_sub(h) >= COINBASE_MATURITY_DEPTH)
        .unwrap_or(false)
}

/// Drop replay-protection entries that can no longer be resubmitted (strictly
/// lower heights) and KEEP every entry at the current height.
///
/// Clearing the whole set on a template swap looks harmless because a swap
/// usually means a new height, but it also fires on a SAME-height reorg. If the
/// chain then flaps back to the original prev-hash the template is byte-identical
/// again, and an emptied set re-admits solutions that were already credited -
/// letting a miner double-count their own shares in the PPLNS window and take
/// payout funds from everyone else.
fn prune_seen(seen: &mut HashSet<(u64, [u8; 32], u32)>, height: u64) {
    seen.retain(|(h, _, _)| *h >= height);
}

/// What became of one settlement chunk. Only `Delivered` is money in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkOutcome {
    /// The node accepted it AND confirms it holds the transaction.
    Delivered,
    /// The node refused it, or accepted the bytes and then never took the
    /// transaction. Nothing was paid and nothing was relayed.
    Failed,
    /// Submitted, but we could not get the node's verdict. It may be in flight,
    /// so it stays in the pending ledger and must NOT be counted as paid.
    Unresolved,
}

/// Running tally of one settlement pass.
///
/// Only chunks the node actually holds may be reported as paid. Counting an
/// attempted chunk tells the operator (and anything built on these logs) that
/// money moved when it did not, which is exactly how a stalled payout goes
/// unnoticed while miners keep mining for nothing.
#[derive(Debug, Default, PartialEq, Eq)]
struct SettleTally {
    recipients: usize,
    units: u64,
    txs: usize,
    failed_txs: usize,
    failed_units: u64,
    unresolved_txs: usize,
    unresolved_units: u64,
}

impl SettleTally {
    fn record(&mut self, outcome: ChunkOutcome, pushed: usize, units: u64) {
        match outcome {
            ChunkOutcome::Delivered => {
                self.recipients += pushed;
                self.units += units;
                self.txs += 1;
            }
            ChunkOutcome::Failed => {
                self.failed_txs += 1;
                self.failed_units += units;
            }
            ChunkOutcome::Unresolved => {
                self.unresolved_txs += 1;
                self.unresolved_units += units;
            }
        }
    }

    /// Did every chunk reach the node? Anything else means money is still owed.
    fn all_delivered(&self) -> bool {
        self.failed_txs == 0 && self.unresolved_txs == 0
    }

    fn attempted(&self) -> bool {
        self.txs > 0 || self.failed_txs > 0 || self.unresolved_txs > 0
    }

    /// The closing line. Never claims a payout happened: a transaction the node
    /// holds is submitted, not paid, until a block includes it.
    fn summary(&self) -> String {
        let mut s = format!(
            "[settle] settlement submitted: {} recipient(s), {} unit(s) across {} tx(s) the node \
             holds (NOT paid until they confirm on-chain)",
            self.recipients, self.units, self.txs
        );
        if self.failed_txs > 0 {
            s.push_str(&format!(
                "; FAILED: {} tx(s) covering {} unit(s) never reached the node and are still owed \
                 (the next cycle re-issues them)",
                self.failed_txs, self.failed_units
            ));
        }
        if self.unresolved_txs > 0 {
            s.push_str(&format!(
                "; UNVERIFIED: {} tx(s) covering {} unit(s) were submitted but the node's verdict \
                 could not be read, so they are NOT counted as paid",
                self.unresolved_txs, self.unresolved_units
            ));
        }
        s
    }
}

/// Drop a payout hash the node definitively does not hold from the shared
/// pending ledger, so the next cycle can re-issue that chunk.
///
/// Safe ONLY for `Admission::Missing`: the node inserts into its mempool before
/// it relays, so a transaction it never inserted was never broadcast either and
/// cannot come back from another peer to be paid twice.
fn forget_unadmitted_payout(pool: &Arc<Mutex<Pool>>, txhash: &str) {
    let shot = {
        let mut p = plock(pool);
        p.settle_pending_txs.retain(|h| h != txhash);
        // Nothing was paid, so nothing is credited: drop the rows too, and the
        // units they carried go straight back into every recipient's PENDING.
        drop_payout(&mut p.payout_records, txhash);
        p.rebuild_inflight();
        p.state_shot(true)
    };
    flush_state(shot);
}

/// Pay every miner their PPLNS share of the pool's spendable balance. Splits the
/// distributable balance over PAYABLE workers only, then submits one or more
/// transactions (chunked to <=PAYOUT_CHUNK actions each) so a large payout is
/// never rejected by the node's 200-action limit. Idempotent across restarts via
/// the persisted `settle_pending_txs`.
fn settle_once(pool: &Arc<Mutex<Pool>>, wallet_file: &str) {
    let (node, counts, pending_txs) = {
        let p = plock(pool);
        (
            p.node.clone(),
            p.pplns.counts(),
            p.settle_pending_txs.clone(),
        )
    };
    let client = http_client();

    // Resolve any outstanding payouts FIRST, using the node's own view of each tx
    // rather than the wallet balance. Correct even if a submit ACK was lost and
    // even though the same wallet keeps earning coinbase income.
    //
    // This runs BEFORE the "nothing to settle" exit on purpose. A pool with an
    // empty share window still has to finish resolving the payouts it already
    // made: they are what turns a miner's IN FLIGHT into PAID, and skipping the
    // poll left a miner's last payout showing as in flight forever.
    //
    // This guard must fail SAFE. Only a definitive node verdict resolves a hash:
    // an unreachable node, a timeout or an answer we cannot parse keeps the hash
    // and skips the cycle, and a payout confirmed shallower than
    // PAYOUT_MATURITY_DEPTH stays tracked because a reorg could still return it
    // to the mempool. Anything else re-opens the double-payout window.
    if !pending_txs.is_empty() {
        let mut still = Vec::new();
        let mut buried: Vec<String> = Vec::new();
        let mut gone: Vec<String> = Vec::new();
        for hx in &pending_txs {
            let j = get_json(&client, &format!("{node}/query/transaction?hash={hx}"));
            // Never slice mid-character: these come off disk, and a corrupt
            // ledger entry must not panic the settlement thread.
            let short = hx.get(..16).unwrap_or(hx);
            match classify_payout_tx(&j) {
                // The node has buried it: this, and ONLY this, is what turns
                // money that was in flight into money that was paid.
                PayoutTxState::Buried(_) => buried.push(hx.clone()),
                PayoutTxState::Gone => {
                    eprintln!(
                        "[settle] payout tx {short} is unknown to the node (rejected or dropped); \
                         this cycle will re-issue it"
                    );
                    gone.push(hx.clone());
                }
                PayoutTxState::Pending => {
                    println!(
                        "[settle] payout tx {short} is still waiting in the node's mempool; \
                         nobody is paid until a block includes it"
                    );
                    still.push(hx.clone());
                }
                PayoutTxState::Confirming(d) => {
                    println!(
                        "[settle] payout tx {short} is only {d} block(s) deep; \
                         waiting for {PAYOUT_MATURITY_DEPTH} before settling again"
                    );
                    still.push(hx.clone());
                }
                PayoutTxState::Unknown => {
                    eprintln!(
                        "[settle] could not determine the state of payout tx {short}; \
                         keeping it and skipping this cycle"
                    );
                    still.push(hx.clone());
                }
            }
        }
        // Credit the buried ones and forget the dead ones in ONE locked step, so
        // a unit is never simultaneously in flight and paid, and never neither.
        if !buried.is_empty() || !gone.is_empty() {
            let (shot, credited) = {
                let mut guard = plock(pool);
                // Reborrow so the two ledgers can be handed over together: they
                // MUST move in one step, or a unit is briefly in both or neither.
                let p = &mut *guard;
                let now = curtimes();
                let mut credited: Vec<(String, u64, usize)> = Vec::new();
                for hx in &buried {
                    if let Some(rec) = confirm_payout(&mut p.payout_records, &mut p.paid, hx, now) {
                        credited.push((hx.clone(), rec.units(), rec.rows.len()));
                    }
                }
                for hx in &gone {
                    drop_payout(&mut p.payout_records, hx);
                }
                p.rebuild_inflight();
                p.settle_pending_txs.retain(|h| still.contains(h));
                (p.state_shot(true), credited)
            };
            flush_state(shot);
            for (hx, units, n) in credited {
                let short = hx.get(..16).unwrap_or(&hx);
                println!(
                    "[settle] payout tx {short} is buried: {units} unit(s) to {n} miner(s) are \
                     now PAID"
                );
            }
        }
        if !still.is_empty() {
            // Some payout is still in flight (or unresolved); keep those and skip.
            let (shot, stalls) = {
                let mut p = plock(pool);
                p.settle_pending_txs = still;
                p.settle_stalls += 1;
                (p.state_shot(true), p.settle_stalls)
            };
            flush_state(shot);
            // A payout that never confirms freezes EVERY later payout, and the
            // old code said nothing at all about it. Say it, and say what to look
            // at: this pool mines coinbase-only blocks, so its own transactions
            // only confirm when some other miner packs them.
            if stalls >= STALLED_PAYOUT_CYCLES {
                eprintln!(
                    "[settle] WARNING: no payout has confirmed for {stalls} settlement cycles, so \
                     nothing is being paid. The blocks this pool mines carry only their coinbase, \
                     so a payout confirms only when another miner includes it - on a chain where \
                     this pool is the only miner it never will. Check that the node is connected \
                     to peers that are mining."
                );
            }
            return;
        }
        // Every prior payout is buried or definitively gone: clear and settle
        // fresh income.
        let shot = {
            let mut p = plock(pool);
            p.settle_pending_txs.clear();
            p.settle_stalls = 0;
            p.state_shot(true)
        };
        flush_state(shot);
    }

    // Nothing credited in the window: nobody is owed anything, so there is
    // nothing to split. The payout resolution above still ran.
    if counts.is_empty() {
        return;
    }

    let acc = load_or_create_wallet(wallet_file);
    let bal = balance(&client, &node, acc.readable());
    // An answer we cannot value is NOT a zero balance: paying out on a garbled or
    // implausible one would sign transactions for a number the node never
    // reported. Skip the cycle instead; the accounting is untouched.
    let Some(units) = balance_units(&bal) else {
        eprintln!(
            "[settle] the node reported a balance this pool cannot value ({bal:?}); \
             skipping this settlement cycle"
        );
        // Say so to miners too: `/earnings` must report PENDING as stale rather
        // than keep quoting a figure the pool can no longer confirm.
        plock(pool).matured_current = false;
        return;
    };
    // Read the hold-back AFTER the balance, never before. A block found in
    // between then appears in the hold-back but not yet in the balance, which
    // errs towards paying LESS; the other order would let a block found during
    // the (slow) pending-payout poll slip through and be paid at 0 confirmations.
    let immature_units: u64 = plock(pool).immature.iter().map(|(_, _, u)| *u).sum();

    // Keep a reserve so the wallet always covers the (per-chunk) tx fee. No pool
    // fee is skimmed: this is a community pool, and the reserve covers the fees.
    // `/terms` reports this same constant, so what a miner is told is what runs.
    let reserve = SETTLE_RESERVE_UNITS;
    // Hold back the coinbase of blocks that are not yet buried: distributing
    // income a reorg can still revoke costs the operator a whole subsidy that
    // nothing can claw back, because the payout stays valid on the new chain.
    let distributable = distributable_units(units, immature_units, reserve);
    // Publish the valuation this cycle just made, so a miner polling /earnings
    // right after a settlement sees the same figure the settlement acted on
    // instead of one up to MONEY_REFRESH_CYCLES old. `None` here is a KNOWN
    // nothing (at or below the reserve), not an unreadable balance.
    {
        let mut p = plock(pool);
        p.matured = Some(Matured {
            units: distributable.unwrap_or(0),
            at: curtimes(),
        });
        p.matured_current = true;
    }
    let Some(distributable) = distributable else {
        if immature_units > 0 {
            println!(
                "[settle] holding back {immature_units} unit(s) of block income that is not yet \
                 buried {COINBASE_MATURITY_DEPTH} deep; nothing matured to pay this cycle"
            );
        }
        return;
    };

    // Split over PAYABLE workers only, so IP-fallback / unpayable keys do not
    // dilute the honest miners' proportional share.
    let payable_counts: Vec<(String, u64)> = counts
        .into_iter()
        .filter(|(w, _)| is_payout_address(w))
        .collect();
    if payable_counts.is_empty() {
        return;
    }
    let split = plan_settlement(distributable, &payable_counts);
    if split.is_empty() {
        return;
    }

    let main = Address::from(*acc.address());
    let mut tally = SettleTally::default();
    for chunk in split.chunks(PAYOUT_CHUNK) {
        // 0.01 HAC network fee, funded by the reserve. Built from the same helper
        // `/terms` quotes, so the fee a miner is told about is the fee the
        // transaction carries.
        let mut tx = TransactionType2::new_by(main.clone(), chunk_tx_fee(), curtimes());
        // Exactly what this transaction pays, in the order it pays it. Only rows
        // that made it into the transaction are here: a recipient the pool had to
        // skip must never appear in anyone's accounting as money in flight.
        let mut rows: Vec<(String, u64)> = Vec::with_capacity(chunk.len());
        for (addr, u) in chunk {
            let Ok(to) = Address::from_readable(addr) else {
                continue;
            };
            let mut act = HacToTrs::new();
            act.to = AddrOrPtr::from_addr(to);
            act.hacash = payout_amount(*u);
            if tx.push_action(Box::new(act)).is_err() {
                break; // should not happen within a <=190 chunk, but stay safe
            }
            rows.push((addr.clone(), *u));
        }
        let pushed = rows.len();
        let chunk_units: u64 = rows.iter().map(|(_, u)| *u).sum();
        if pushed == 0 {
            continue;
        }
        if tx.fill_sign(&acc).is_err() {
            eprintln!("[settle] signing failed for a chunk; skipping it");
            continue;
        }
        // Record the payout tx hash BEFORE submitting AND persist it, so a lost
        // ACK or a crash mid-settlement still blocks a second payout: next cycle
        // we poll this hash and only retry if it is gone.
        //
        // The per-recipient rows are written in the SAME snapshot, not after the
        // node answers. A crash in between would otherwise leave a tracked hash
        // with no rows behind it, and when that payout later confirmed the pool
        // could not tell a single miner it had been paid.
        let txhash = hex::encode(tx.hash().serialize());
        let shot = {
            let mut p = plock(pool);
            p.settle_pending_txs.push(txhash.clone());
            p.payout_records.push(PayoutRecord {
                hash: txhash.clone(),
                at: curtimes(),
                // Not yet: the node has not been asked whether it holds this.
                node_holds: false,
                rows,
            });
            p.rebuild_inflight();
            p.state_shot(true)
        };
        // The fsync happens here, with the pool lock released: no miner request
        // waits on the state disk, and nothing is submitted before it lands.
        let recorded = flush_state(shot);
        if !recorded {
            // An untracked payout is one a later cycle could pay all over again.
            // Nothing was submitted, so stopping here loses nothing; the hash
            // stays in memory and next cycle's poll resolves it as gone.
            eprintln!(
                "[settle] could not record the payout tx on disk; NOT submitting this chunk \
                 (an untracked payout could be paid twice)"
            );
            break;
        }
        let body = hex::encode(tx.serialize());
        let resp = post_hex(
            &client,
            &format!("{node}/submit/transaction?hexbody=true"),
            &body,
        );
        // Surface a node rejection instead of silently reporting success.
        let short = &txhash[..txhash.len().min(16)];
        let accepted = serde_json::from_str::<serde_json::Value>(&resp)
            .ok()
            .and_then(|v| find_u64(&v, "ret"))
            == Some(0);
        if !accepted {
            eprintln!("[settle] node did NOT accept payout tx {short} ({pushed} recipients): {resp}");
            // Never submitted, so never relayed: drop it so the next cycle can
            // re-issue this chunk instead of waiting on a hash nothing holds.
            forget_unadmitted_payout(pool, &txhash);
            tally.record(ChunkOutcome::Failed, pushed, chunk_units);
            continue;
        }
        // ret=0 only means the API took the bytes. The node validates
        // synchronously and then inserts into the mempool on a background task
        // whose result it DISCARDS, so a transaction that fails there is
        // reported as accepted and simply never exists. Ask the node what it
        // actually holds before counting a single unit as sent.
        match verify_admitted(&client, &node, &txhash) {
            Admission::Held => {
                println!(
                    "[settle] submitted payout tx {short} paying {pushed} miner(s) \
                     {chunk_units} units; the node holds it"
                );
                // Upgrade the record: the node confirms it holds this, so every
                // recipient can be told its money is really in flight rather than
                // merely submitted. Still NOT paid - that needs burial.
                let shot = {
                    let mut p = plock(pool);
                    if let Some(r) = p.payout_records.iter_mut().find(|r| r.hash == txhash) {
                        r.node_holds = true;
                    }
                    p.state_shot(true)
                };
                flush_state(shot);
                tally.record(ChunkOutcome::Delivered, pushed, chunk_units);
            }
            Admission::Missing => {
                eprintln!(
                    "[settle] payout tx {short} was accepted by the API but the node does NOT \
                     hold it ({pushed} recipients, {chunk_units} units): nothing was paid and \
                     nothing was relayed. Re-issuing on the next cycle."
                );
                forget_unadmitted_payout(pool, &txhash);
                tally.record(ChunkOutcome::Failed, pushed, chunk_units);
            }
            Admission::Unresolved => {
                eprintln!(
                    "[settle] could not confirm the node holds payout tx {short} \
                     ({pushed} recipients, {chunk_units} units); keeping it in the pending \
                     ledger and NOT counting it as paid"
                );
                tally.record(ChunkOutcome::Unresolved, pushed, chunk_units);
            }
        }
    }
    if !tally.attempted() {
        return;
    }
    // Loud on anything that did not reach the node: an operator reading only the
    // closing line must never be told money moved when it did not.
    if tally.all_delivered() {
        println!("{}", tally.summary());
    } else {
        eprintln!("{}", tally.summary());
    }
}

/// Wraps the share-credit path: takes the pool lock only twice (a brief snapshot
/// and a brief commit), computing the expensive x16rs hash OFF the lock so one
/// submission cannot serialize every other miner behind a full PoW evaluation.
/// Replay protection stays atomic: the (height, coinbase_nonce, block_nonce) key
/// is inserted under the commit lock and a second submission of the same key is
/// rejected there.
fn handle_submission(
    pool: &Arc<Mutex<Pool>>,
    worker: &str,
    height: u64,
    coinbase_nonce: [u8; 32],
    block_nonce: u32,
) -> serde_json::Value {
    // No route may seat an unpayable key in the PPLNS window. The window is a
    // fixed 4096 shares shared by everyone, so a key that is filtered out at
    // payout still evicts shares from miners the pool CAN pay - their work is
    // credited for a shorter stretch and small/sporadic miners fall out of the
    // window before a block is found. The paid path already refuses these; this
    // is the backstop for every other caller.
    if !is_payout_address(worker) {
        return json!({
            "ok": false,
            "kind": "invalid",
            "err": "set worker=<your HAC address> so the pool can pay you"
        });
    }
    let key = (height, coinbase_nonce, block_nonce);
    // Phase 1 — brief lock: reject stale/duplicate early and snapshot the inputs.
    let (tpl, share_target, network_target, client, node) = {
        let p = plock(pool);
        if height != p.tpl.height {
            return json!({"ok":false,"kind":"stale","height":p.tpl.height});
        }
        if p.seen.contains(&key) {
            return json!({"ok":false,"kind":"duplicate"});
        }
        (
            p.tpl.clone(),
            p.share_target,
            p.network_target,
            p.client.clone(),
            p.node.clone(),
        )
    };

    // Phase 2 — no lock: rebuild exactly what the worker hashed and evaluate the
    // (deliberately slow) x16rs PoW hash without blocking any other request.
    let cb = coinbase_with_extranonce(&tpl, &coinbase_nonce);
    let intro = intro_bytes(&tpl, &cb, block_nonce);
    let hash = pool_core::hash_of(tpl.height, &intro);
    if !pool_core::beats(&hash, &share_target) {
        // The pool never trusts a worker's own header: it rebuilds one from
        // (height, coinbase_nonce, block_nonce) and hashes THAT, so a worker
        // that computes a different merkle root has its shares rejected rather
        // than credited. It cannot steal - but it also cannot earn, and a silent
        // reject counter is how a miner burns a day of hashrate for nothing.
        if let Some(streak) = plock(pool).note_bad_share(worker) {
            eprintln!(
                "[{worker}] {streak} shares IN A ROW hashed above the share target. The pool \
                 rebuilds every share's header itself, so this worker is hashing a DIFFERENT \
                 header than the pool: the usual cause is a worker that ignores the \
                 `mkrl_modify_list` in /query/miner/pending and so builds a different merkle \
                 root. Nothing this worker submits can be credited until that is fixed."
            );
        }
        return json!({"ok":false,"kind":"invalid","err":"above share target"});
    }
    let is_block = pool_core::beats(&hash, &network_target);

    // Phase 3 — brief lock: atomically re-check freshness + replay, then credit.
    // The accounting snapshot leaves the lock as bytes; writing it is phase 3b,
    // because every other request is serialized behind this same mutex and a
    // create/rename/fsync must never happen underneath it.
    let (commit, shot) = {
        let mut p = plock(pool);
        if height != p.tpl.height {
            return json!({"ok":false,"kind":"stale","height":p.tpl.height});
        }
        // The rate limiter exists to bound the replay set, not to throw money
        // away: a submission that beats the NETWORK target is a whole block
        // reward, so it is evaluated FIRST and is never refused here. Only
        // ordinary shares are shed at the cap.
        if share_limiter_rejects(p.seen.len(), is_block) {
            return json!({"ok":false,"kind":"busy","err":"too many shares this height"});
        }
        if !p.seen.insert(key) {
            return json!({"ok":false,"kind":"duplicate"});
        }
        p.pplns.record(worker);
        p.note_good_share(worker);
        p.accepted += 1;
        if !is_block {
            (Commit::Share(p.accepted), p.note_share_saved())
        } else {
            // Serializing the block is deliberately NOT done here: it now copies
            // the node's whole transaction set, and every other miner's request
            // is serialized behind this mutex. It needs nothing but the phase-1
            // snapshot, so phase 4 builds it with the lock released.
            let solved = tpl.height;
            p.submitted.push((solved, hash)); // counted once the bg thread sees it stick
            // Hold this block's coinbase back from settlement until the chain has
            // buried it. The node credits the reward the moment the block is
            // inserted, so without this the very next settle tick would pay out
            // income that is 0-1 confirmations deep.
            p.immature.push((solved, hash, block_reward_units(solved)));
            (Commit::Block, p.state_shot(true))
        }
    };

    // Phase 3b - no lock: persist the accounting (fsync on a block) before the
    // block goes out, so a crash right after submitting still knows about it.
    flush_state(shot);

    match commit {
        Commit::Share(accepted) => {
            return json!({"ok":true,"kind":"share","accepted":accepted});
        }
        Commit::Block => {}
    }

    // Phase 4 - no lock: serialize and submit the winning block. This is where
    // the node's packed transactions are carried into the block: OUR coinbase in
    // slot 0, then every transaction the node packed for this height, with a
    // merkle root folded from the node's own sibling list.
    let block_bytes = assemble_block(&tpl, &cb, block_nonce);
    let packed = tpl.txs.bodies.len();
    let submit = submit_block_bytes(&client, &node, &block_bytes);
    // The submit answer used to go only into the JSON the winning worker reads,
    // so an outright refusal - a whole block reward - never reached the operator.
    if block_submit_refused(&submit) {
        eprintln!(
            "[block] the node REFUSED our block at height {height} ({packed} packed tx(s), {} \
             bytes): {submit}. That block's entire reward is lost.",
            block_bytes.len()
        );
    } else {
        println!(
            "[block] submitted height {height} carrying {packed} packed tx(s) ({} bytes): \
             {submit}",
            block_bytes.len()
        );
    }
    json!({"ok":true,"kind":"block","solved_height":height,"submit":submit})
}

/// Did `/submit/block` refuse the block outright?
///
/// The node validates asynchronously, so `ret:0` means only "parsed and queued"
/// and is NOT proof of acceptance - `note_block_stalls` is what catches a later
/// silent refusal. But `ret:1`, a transport failure, or an unparseable answer
/// are definitive, and each one costs a whole block reward.
fn block_submit_refused(resp: &str) -> bool {
    match serde_json::from_str::<serde_json::Value>(resp) {
        Ok(j) => find_u64(&j, "ret") != Some(0),
        Err(_) => true,
    }
}

/// Should the per-height share rate limiter refuse this submission?
///
/// The cap bounds the replay set for one height. Applying it before the block
/// check meant that at the cap the pool answered "busy" to a solution that beat
/// the NETWORK target and threw away a whole block reward - the most valuable
/// thing a miner can hand it. A network block is one entry, is vanishingly rare
/// next to the cap, and is worth far more than the memory it costs, so it is
/// always admitted.
fn share_limiter_rejects(seen_len: usize, is_block: bool) -> bool {
    !is_block && seen_len >= SEEN_CAP
}

/// What phase 3 of a submission decided, carried out of the lock scope so the
/// answer is built (and the state written, and the block serialized) with the
/// pool mutex released.
enum Commit {
    Share(u64),
    Block,
}

fn hash32(s: &str) -> Option<[u8; 32]> {
    let v = hex::decode(s).ok()?;
    if v.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Some(out)
}

fn parse32(s: Option<&String>) -> Option<[u8; 32]> {
    hash32(s?)
}

/// Read the HTTP request line under BOTH a size cap and an ABSOLUTE wall-clock
/// deadline.
///
/// A socket read timeout bounds each read syscall, not the request: a client
/// dribbling one byte every few seconds resets the timer on every byte and can
/// hold a handler thread (plus its global and per-IP slot) for hours, which is
/// all a slow-loris needs to starve every real miner. Checking elapsed time
/// after each read is what actually bounds it.
fn read_request_line(s: TcpStream) -> Option<String> {
    let start = Instant::now();
    let mut reader = BufReader::new(s.take(MAX_REQUEST_LINE as u64));
    let mut line: Vec<u8> = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        if start.elapsed() >= REQUEST_READ_DEADLINE {
            return None;
        }
        match reader.read(&mut byte) {
            Ok(0) => return None, // peer closed before sending a full line
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                if line.len() >= MAX_REQUEST_LINE {
                    return None;
                }
                line.push(byte[0]);
            }
            // A per-read timeout is not fatal on its own; the deadline above is
            // what ends the connection. Any other error is.
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return None,
        }
    }
    String::from_utf8(line).ok()
}

fn handle(mut s: TcpStream, pool: Arc<Mutex<Pool>>) {
    // Bound how long a client may hold a connection and how much we read, so a
    // slow-loris or a socket that never sends a newline cannot pin a thread or
    // grow memory without limit. The request line we care about is tiny. The
    // per-read timeout is short so the absolute deadline is honoured promptly.
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
    let Ok(peek) = s.try_clone() else { return };
    let Some(line) = read_request_line(peek) else {
        return;
    };
    let target = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let params = parse_query(&query);
    // The standard miner API carries no worker id, so attribute by source IP.
    let peer = s
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let body = route(&path, &params, &pool, &peer);
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = s.write_all(resp.as_bytes());
    let _ = s.flush();
    // Orderly close: signal end-of-write, then drain any unread request bytes so
    // the peer's pending data does not RST-truncate our response (seen on Windows).
    let _ = s.shutdown(Shutdown::Write);
    let mut sink = [0u8; 2048];
    let _ = s.read(&mut sink);
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn route(
    path: &str,
    params: &HashMap<String, String>,
    pool: &Arc<Mutex<Pool>>,
    peer: &str,
) -> String {
    match path {
        // ---- standard Hacash miner API: an UNMODIFIED poworker mines here ----
        "/query/miner/pending" => plock(pool).pending_cache.clone(),
        "/query/miner/notice" => {
            let want: u64 = params.get("height").and_then(|v| v.parse().ok()).unwrap_or(0);
            let wait: u64 = params
                .get("wait")
                .and_then(|v| v.parse().ok())
                .unwrap_or(45)
                .clamp(1, 120);
            // Budget long-polls separately: if too many are already parked, answer
            // immediately with the current height rather than holding another slot.
            if NOTICE_WAITERS.fetch_add(1, Relaxed) >= MAX_NOTICE_WAITERS {
                NOTICE_WAITERS.fetch_sub(1, Relaxed);
                let h = plock(pool).tpl.height;
                return json!({"ret":0,"height":h}).to_string();
            }
            let _ng = NoticeGuard;
            let deadline = Instant::now() + Duration::from_secs(wait);
            loop {
                let h = plock(pool).tpl.height; // brief lock only
                if h > want || Instant::now() >= deadline {
                    return json!({"ret":0,"height":h}).to_string();
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        }
        "/submit/miner/success" => {
            let height: u64 = params.get("height").and_then(|v| v.parse().ok()).unwrap_or(0);
            let block_nonce: u32 = params
                .get("block_nonce")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let Some(cn) = parse32(params.get("coinbase_nonce")) else {
                return json!({"ret":1,"err":"bad coinbase_nonce"}).to_string();
            };
            // A share is only worth crediting if we can pay it. Require the miner
            // to announce a payable address; crediting an IP-fallback key that is
            // then dropped at payout would silently mine for nothing.
            let Some(worker) = params
                .get("worker")
                .filter(|w| is_payout_address(w))
                .cloned()
            else {
                return json!({
                    "ret": 1,
                    "err": "set pool_worker=<your HAC address> so the pool can pay you"
                })
                .to_string();
            };
            let _ = peer; // no longer used for attribution on the paid path
            let r = handle_submission(pool, &worker, height, cn, block_nonce);
            let ok = r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            let kind = r.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if ok {
                println!("[{worker}] {kind} at height {height}");
                json!({"ret":0,"kind":kind}).to_string()
            } else {
                json!({"ret":1,"kind":kind}).to_string()
            }
        }

        // ---- our own simple protocol (hbit-test-miner) ----
        // Both routes demand a payable worker exactly like the paid path: a share
        // credited to a key the pool cannot pay is work done for nothing that
        // also evicts a payable miner's share from the shared 4096-share window.
        "/work" => {
            let Some(worker) = params.get("worker").filter(|w| is_payout_address(w)).cloned()
            else {
                return json!({
                    "ok": false,
                    "err": "set worker=<your HAC address> so the pool can pay you"
                })
                .to_string();
            };
            let mut p = plock(pool);
            let en = p.extranonce_for(&worker);
            let cb = coinbase_with_extranonce(&p.tpl, &en);
            let intro = intro_bytes(&p.tpl, &cb, 0);
            json!({
                "ok": true,
                "height": p.tpl.height,
                "intro": hex::encode(intro),
                "share_target": hex::encode(p.share_target),
                "network_target": hex::encode(p.network_target),
                "extranonce": hex::encode(en),
            })
            .to_string()
        }
        "/share" => {
            let Some(worker) = params.get("worker").filter(|w| is_payout_address(w)).cloned()
            else {
                return json!({
                    "ok": false,
                    "kind": "invalid",
                    "err": "set worker=<your HAC address> so the pool can pay you"
                })
                .to_string();
            };
            let height: u64 = params.get("height").and_then(|v| v.parse().ok()).unwrap_or(0);
            let nonce: u32 = params.get("nonce").and_then(|v| v.parse().ok()).unwrap_or(0);
            let en = {
                let p = plock(pool);
                match p.workers.get(&worker).copied() {
                    Some(en) => en,
                    None => {
                        return json!({"ok":false,"kind":"invalid","err":"unknown worker"})
                            .to_string();
                    }
                }
            };
            handle_submission(pool, &worker, height, en, nonce).to_string()
        }
        "/stats" => {
            // Copy the numbers out, then RELEASE the lock before building the
            // body. /stats is open and unauthenticated, and serializing up to
            // PPLNS_WINDOW worker rows under the global mutex would let anyone
            // stall every miner's /work, /share and /submit by polling it.
            let (height, difficulty, accepted, blocks, pending, orphaned, window, workers) = {
                let p = plock(pool);
                (
                    p.tpl.height,
                    p.tpl.difficulty,
                    p.accepted,
                    p.blocks,
                    p.submitted.len(),
                    p.orphaned,
                    p.pplns.total(),
                    p.pplns.counts(),
                )
            };
            json!({
                "height": height,
                "difficulty": difficulty,
                "accepted_shares": accepted,
                "blocks_confirmed": blocks,
                "blocks_pending": pending,
                "blocks_orphaned": orphaned,
                "share_window": window,
                "workers": workers,
            })
            .to_string()
        }

        // The pool's terms, READ OUT OF the code that enforces them. Nothing here
        // is a number somebody typed into a description: change what the pool
        // does and this changes with it, which is the whole point.
        "/terms" => {
            let (window_size, share_factor, achieved, difficulty, settle_secs) = {
                let p = plock(pool);
                (
                    p.pplns.window() as u64,
                    p.share_factor,
                    p.share_factor_achieved,
                    p.tpl.difficulty,
                    p.settle_secs,
                )
            };
            terms_json(
                window_size,
                share_factor,
                achieved,
                difficulty,
                settle_secs,
                COINBASE_MATURITY_DEPTH,
                PAYOUT_MATURITY_DEPTH,
            )
            .to_string()
        }

        // What ONE worker is owed, what is in flight for it, and what it has been
        // paid. Polled by miners, so it does no node call and holds the pool lock
        // only long enough to copy this worker's own numbers out.
        "/earnings" => {
            let Some(worker) = params.get("worker") else {
                return json!({
                    "ok": false,
                    "kind": "missing_worker",
                    "err": "ask for one worker: /earnings?worker=<your HAC address>",
                })
                .to_string();
            };
            match classify_worker_id(worker) {
                WorkerId::NotAnAddress => json!({
                    "ok": false,
                    "kind": "invalid_address",
                    "worker": worker,
                    "err": "that is not a Hacash address",
                })
                .to_string(),
                WorkerId::Unpayable => json!({
                    "ok": false,
                    "kind": "unpayable_address",
                    "worker": worker,
                    "err": "that is a Hacash address this pool cannot pay, so it never credits \
                            shares to it. Use a normal single-key address.",
                })
                .to_string(),
                WorkerId::Payable => {
                    let e = plock(pool).earnings_of(worker);
                    earnings_json(worker, &e, curtimes()).to_string()
                }
            }
        }
        _ => json!({"ok":false,"err":"no such endpoint"}).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use basis::difficulty::LOWEST_DIFFICULTY;
    use hbit_pool::{PackedTxs, PaidRow, admission_of};
    use std::sync::Arc;

    /* ---- the money a miner is shown: paid, in flight, pending ---- */

    /// Two real, payable mainnet-format addresses. The pool refuses a share from
    /// anything else, so the accounting keys are always addresses like these.
    const W_A: &str = "1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9";
    const W_B: &str = "12vi7DEZjh6KrK5PVmmqSgvuJPCsZMmpfi";

    fn a_payout(hash: &str, at: u64, node_holds: bool, rows: &[(&str, u64)]) -> PayoutRecord {
        PayoutRecord {
            hash: hash.to_string(),
            at,
            node_holds,
            rows: rows.iter().map(|(w, u)| (w.to_string(), *u)).collect(),
        }
    }

    fn inflight_total(records: &[PayoutRecord]) -> u64 {
        records.iter().map(|r| r.units()).sum()
    }

    #[test]
    fn the_three_money_buckets_never_double_count() {
        // Follow ONE worker's 40 units through the whole lifecycle. At every step
        // the three buckets must add up to the same 40, and no unit may ever be
        // in two of them: that is the entire promise /earnings makes.
        let mut records: Vec<PayoutRecord> = Vec::new();
        let mut paid = PaidLedger::started(1_000);
        // What the pool's wallet says has matured and is unsettled. A payout
        // sitting in the mempool does NOT reduce the node's confirmed balance, so
        // this number keeps containing money that is already in flight - which is
        // exactly why pending has to subtract it.
        let mut matured = 40u64;

        let buckets = |matured: u64, records: &[PayoutRecord], paid: &PaidLedger| {
            let inflight = inflight_total(records);
            let pending = matured.saturating_sub(inflight);
            let done = paid.get(W_A).map(|r| r.units).unwrap_or(0);
            (pending, inflight, done)
        };

        // 1) Nothing submitted yet: it is all pending.
        assert_eq!(buckets(matured, &records, &paid), (40, 0, 0));

        // 2) Submitted. The same 40 units are in flight, and pending must drop to
        // zero even though the confirmed balance still holds them.
        records.push(a_payout("tx1", 1_100, true, &[(W_A, 40)]));
        assert_eq!(buckets(matured, &records, &paid), (0, 40, 0));

        // 3) Buried. It leaves in flight at the same instant it enters paid, and
        // the chain has really spent it so the balance falls with it.
        let rec = confirm_payout(&mut records, &mut paid, "tx1", 1_200).expect("credited");
        assert_eq!(rec.units(), 40);
        matured -= 40;
        assert_eq!(buckets(matured, &records, &paid), (0, 0, 40));

        // Every step totalled 40. Nothing was created and nothing was lost.
        for (p, i, d) in [(40, 0, 0), (0, 40, 0), (0, 0, 40)] {
            assert_eq!(p + i + d, 40);
        }
    }

    #[test]
    fn a_payout_the_node_never_took_goes_back_to_pending_and_never_to_paid() {
        let mut records = vec![a_payout("dead", 1_000, false, &[(W_A, 25)])];
        let mut paid = PaidLedger::started(1_000);
        let matured = 25u64;
        assert_eq!(matured.saturating_sub(inflight_total(&records)), 0);

        // The node definitively does not hold it: nothing was paid and nothing
        // was relayed, so those units are owed again.
        assert!(drop_payout(&mut records, "dead").is_some());
        assert_eq!(inflight_total(&records), 0);
        assert_eq!(matured.saturating_sub(inflight_total(&records)), 25);
        assert!(paid.get(W_A).is_none(), "a dropped payout pays nobody");
        // And it cannot be confirmed after the fact: there is nothing to confirm.
        assert!(confirm_payout(&mut records, &mut paid, "dead", 2_000).is_none());
        assert!(paid.get(W_A).is_none());
    }

    #[test]
    fn paid_moves_only_when_the_node_buries_a_payout_and_only_upward() {
        let mut records: Vec<PayoutRecord> = Vec::new();
        let mut paid = PaidLedger::started(1_000);

        // Submitted is not paid, however sure the node sounds.
        records.push(a_payout("tx1", 1_100, true, &[(W_A, 10), (W_B, 5)]));
        assert_eq!(paid.total_units(), 0);
        assert_eq!(paid.workers(), 0);

        // Buried: credited, once, to the right people in the right amounts.
        confirm_payout(&mut records, &mut paid, "tx1", 1_200).expect("credited");
        assert_eq!(paid.get(W_A).unwrap().units, 10);
        assert_eq!(paid.get(W_B).unwrap().units, 5);
        assert_eq!(paid.total_units(), 15);
        assert_eq!(paid.get(W_A).unwrap().last_hash, "tx1");
        assert_eq!(paid.get(W_A).unwrap().last_at, 1_200);

        // A second payout adds; it never replaces.
        records.push(a_payout("tx2", 2_100, true, &[(W_A, 7)]));
        confirm_payout(&mut records, &mut paid, "tx2", 2_200).expect("credited");
        assert_eq!(paid.get(W_A).unwrap().units, 17, "totals only ever grow");
        assert_eq!(paid.get(W_B).unwrap().units, 5, "untouched by someone else's payout");
        // "last payout" is the most recent one, with its own amount and hash.
        assert_eq!(paid.get(W_A).unwrap().last_units, 7);
        assert_eq!(paid.get(W_A).unwrap().last_hash, "tx2");
        assert_eq!(paid.get(W_A).unwrap().last_at, 2_200);

        // A restart must not lose or double any of it.
        let restored = PaidLedger::from_json(&paid.to_json());
        assert_eq!(restored, paid);
        assert_eq!(restored.get(W_A).unwrap().units, 17);
        assert_eq!(restored.since, 1_000);
    }

    #[test]
    fn a_payout_row_survives_the_state_file_round_trip() {
        // The rows are what let a confirmed payout be credited to the right
        // miners. Losing them across a restart means a payout that confirms and
        // reaches nobody's total.
        let rec = a_payout("abc123", 4_242, true, &[(W_A, 12), (W_B, 3)]);
        let back = PayoutRecord::from_json(&rec.to_json()).expect("parsed");
        assert_eq!(back, rec);
        assert_eq!(back.units(), 15);
        assert_eq!(back.units_for(W_A), 12);
        assert_eq!(back.units_for("someone-else"), 0);
        // A row the file cannot describe is dropped, never guessed at.
        let broken = serde_json::json!({"hash":"h","at":1,"rows":[["a"],["b",4]]});
        let back = PayoutRecord::from_json(&broken).expect("parsed");
        assert_eq!(back.rows, vec![("b".to_string(), 4u64)]);
        assert!(!back.node_holds, "an absent verdict is not a confirmed one");
        assert!(PayoutRecord::from_json(&serde_json::json!({"at":1})).is_none());
    }

    /* ---- what /earnings says, and what it refuses to say ---- */

    fn an_earnings() -> Earnings {
        Earnings {
            known: true,
            shares: 0,
            window_shares: 0,
            window_size: PPLNS_WINDOW as u64,
            paid: PaidRow::default(),
            paid_since: 1_000,
            inflight_units: 0,
            inflight: Vec::new(),
            pool_pending_units: Some(0),
            matured_at: 1_500,
            matured_current: true,
            unattributed_payouts: 0,
        }
    }

    #[test]
    fn an_unknown_worker_is_not_a_worker_that_is_owed_nothing() {
        // Showing "0 HAC owed" for an address the pool has never seen tells a
        // miner its work is being tracked when it is not.
        let unknown = Earnings {
            known: false,
            ..an_earnings()
        };
        let j = earnings_json(W_A, &unknown, 2_000);
        assert_eq!(j["kind"].as_str(), Some("unknown_worker"));
        assert_eq!(j["known"].as_bool(), Some(false));
        for absent in ["paid", "pending", "in_flight", "last_payout"] {
            assert!(j.get(absent).is_none(), "no money figures at all: {absent}");
        }

        // A worker the pool DOES know, owed nothing, says exactly that.
        let zero = an_earnings();
        let j = earnings_json(W_A, &zero, 2_000);
        assert_eq!(j["kind"].as_str(), Some("worker"));
        assert_eq!(j["known"].as_bool(), Some(true));
        assert_eq!(j["paid"]["units"].as_u64(), Some(0));
        assert_eq!(j["pending"]["known"].as_bool(), Some(true));
        assert_eq!(j["pending"]["units"].as_u64(), Some(0));
        assert_eq!(j["in_flight"]["units"].as_u64(), Some(0));
        assert!(j["last_payout"].is_null(), "never paid is null, not 0");
    }

    #[test]
    fn a_bad_address_is_a_third_answer_again() {
        assert_eq!(classify_worker_id(W_A), WorkerId::Payable);
        assert_eq!(classify_worker_id("not-an-address"), WorkerId::NotAnAddress);
        assert_eq!(classify_worker_id(""), WorkerId::NotAnAddress);
        // A well-formed address this pool could never pay is its own case: the
        // pool refuses to credit shares to it at all, so reporting it as an
        // unknown worker would hide WHY it will never earn anything.
        let mut raw = [0u8; 21];
        raw[0] = 1; // CONTRACT version
        raw[1..].copy_from_slice(&[9u8; 20]);
        let contract = Address::from(raw).to_readable();
        assert_eq!(classify_worker_id(&contract), WorkerId::Unpayable);
    }

    #[test]
    fn pending_is_unknown_not_zero_when_the_pool_cannot_value_its_wallet() {
        // "0 HAC owed" and "the pool cannot say" are different facts, and the
        // second must never be rendered as the first.
        let blind = Earnings {
            pool_pending_units: None,
            shares: 10,
            window_shares: 10,
            ..an_earnings()
        };
        let j = earnings_json(W_A, &blind, 2_000);
        assert_eq!(j["pending"]["known"].as_bool(), Some(false));
        assert!(j["pending"].get("units").is_none(), "no number to misread");
        assert!(j["pending"].get("amount").is_none());
        assert!(
            j["pending"]["reason"].as_str().unwrap().contains("not a zero"),
            "{}",
            j["pending"]
        );
        // The rest of the answer still stands: paid and in flight are the pool's
        // own ledger and do not depend on reading the wallet.
        assert_eq!(j["paid"]["units"].as_u64(), Some(0));

        // A known zero says so, and says how old the figure is.
        let known_zero = Earnings {
            pool_pending_units: Some(0),
            shares: 10,
            window_shares: 10,
            ..an_earnings()
        };
        let j = earnings_json(W_A, &known_zero, 2_000);
        assert_eq!(j["pending"]["known"].as_bool(), Some(true));
        assert_eq!(j["pending"]["units"].as_u64(), Some(0));
        assert_eq!(j["pending"]["estimate"].as_bool(), Some(true));
        assert_eq!(j["pending"]["as_of_age_secs"].as_u64(), Some(500));
        assert_eq!(j["pending"]["current"].as_bool(), Some(true));
    }

    #[test]
    fn pending_is_unknown_while_a_tracked_payout_has_no_recipient_detail() {
        // A payout hash left by an older build, or by a tool that recorded only
        // the hash. Its units are still inside the node's confirmed balance, so
        // the in-flight subtraction is incomplete and pending would be too HIGH -
        // the one direction that promises a miner money.
        let e = Earnings {
            shares: 1,
            window_shares: 1,
            pool_pending_units: Some(100),
            unattributed_payouts: 1,
            ..an_earnings()
        };
        let j = earnings_json(W_A, &e, 2_000);
        assert_eq!(j["pending"]["known"].as_bool(), Some(false));
        assert!(j["pending"].get("units").is_none());
        let why = j["pending"]["reason"].as_str().expect("a reason");
        assert!(why.contains("not a zero"), "{why}");
        assert!(why.contains("overstate"), "{why}");
        // Paid and in flight are the pool's own ledger and still stand.
        assert_eq!(j["paid"]["units"].as_u64(), Some(0));
        // Once the rows are known, the same worker gets its figure.
        let ok = Earnings {
            unattributed_payouts: 0,
            ..e
        };
        let j = earnings_json(W_A, &ok, 2_000);
        assert_eq!(j["pending"]["units"].as_u64(), Some(100));
    }

    #[test]
    fn money_is_reported_as_the_chains_own_amount() {
        // Never a float, never a decimal this pool rounded itself: the same
        // mantissa:unit string the node speaks and the transaction carries.
        let e = Earnings {
            paid: PaidRow {
                units: 35,
                last_units: 35,
                last_hash: "tx1".to_string(),
                last_at: 1_900,
            },
            inflight_units: 4,
            inflight: vec![InflightRow {
                hash: "tx2".to_string(),
                units: 4,
                at: 1_950,
                node_holds: true,
            }],
            shares: 2,
            window_shares: 4,
            pool_pending_units: Some(10),
            ..an_earnings()
        };
        let j = earnings_json(W_A, &e, 2_000);
        assert_eq!(j["paid"]["amount"].as_str(), Some("35:247"));
        assert_eq!(j["paid"]["units"].as_u64(), Some(35));
        assert_eq!(j["last_payout"]["amount"].as_str(), Some("35:247"));
        assert_eq!(j["last_payout"]["tx"].as_str(), Some("tx1"));
        assert_eq!(j["last_payout"]["at_unix"].as_u64(), Some(1_900));
        assert_eq!(j["in_flight"]["amount"].as_str(), Some("4:247"));
        assert_eq!(j["in_flight"]["txs"][0]["node_holds"].as_bool(), Some(true));
        // 10 units over a window of 4, holding 2 of them: 5.
        assert_eq!(j["pending"]["units"].as_u64(), Some(5));
        assert_eq!(j["pending"]["amount"].as_str(), Some("5:247"));
        // The chain normalizes 100 units of 0.1 HAC to 10 HAC, and so does this.
        assert_eq!(payout_amount(100).to_fin_string(), "1:249");
        assert_eq!(payout_amount(0).to_fin_string(), "0:0");
    }

    #[test]
    fn a_pending_estimate_never_promises_more_than_a_settlement_would_pay() {
        // The estimate is the floor share. `split_payout` may hand one extra unit
        // to the largest remainders, so the estimate is at or below what a
        // settlement at this instant pays - never above it.
        let counts = vec![(W_A.to_string(), 3u64), (W_B.to_string(), 1u64)];
        let split: HashMap<String, u64> = plan_settlement(100, &counts).into_iter().collect();
        assert_eq!(worker_pending_units(100, 3, 4), split[W_A]);
        assert_eq!(worker_pending_units(100, 1, 4), split[W_B]);

        let thirds = vec![
            (W_A.to_string(), 1u64),
            (W_B.to_string(), 1u64),
            ("c".to_string(), 1u64),
        ];
        for (_, paid) in plan_settlement(100, &thirds) {
            assert!(
                worker_pending_units(100, 1, 3) <= paid,
                "the estimate must never be the rounded-UP share"
            );
        }

        // Nothing to split, nothing held, or an empty window: zero, honestly.
        assert_eq!(worker_pending_units(0, 5, 10), 0);
        assert_eq!(worker_pending_units(100, 0, 10), 0);
        assert_eq!(worker_pending_units(100, 5, 0), 0);
        // A share below the advertised minimum is paid nothing at all, so the
        // estimate must not promise a fraction of it.
        assert_eq!(worker_pending_units(1, 1, 1_000), 0);
        // And it does not overflow at the top of the range.
        assert_eq!(worker_pending_units(u64::MAX, 1, 1), u64::MAX);
    }

    /// A pool with no node, no disk and no listener: enough to exercise the
    /// accounting the endpoints read.
    fn a_pool() -> Pool {
        let tpl = a_template(PackedTxs::default());
        Pool {
            node: String::new(),
            payout: W_A.to_string(),
            state_file: String::new(), // no disk: state_shot returns None
            client: http_client(),
            params: ChainParams::mainnet(),
            share_target: [0xff; 32],
            network_target: tpl.target,
            tpl,
            share_factor: 24,
            share_factor_achieved: 24,
            pending_cache: String::new(),
            workers: HashMap::new(),
            next_en: 0,
            pplns: Pplns::new(PPLNS_WINDOW),
            accepted: 0,
            blocks: 0,
            orphaned: 0,
            seen: HashSet::new(),
            submitted: Vec::new(),
            immature: Vec::new(),
            unsaved: 0,
            state_seq: 0,
            settle_pending_txs: Vec::new(),
            payout_records: Vec::new(),
            inflight_units: 0,
            paid: PaidLedger::started(1_000),
            matured: None,
            matured_current: false,
            settle_secs: 300,
            settle_stalls: 0,
            bad_streak: HashMap::new(),
        }
    }

    #[test]
    fn a_workers_pending_never_repeats_money_that_is_already_in_flight() {
        // The node's CONFIRMED balance still contains a payout that is sitting in
        // the mempool. Reporting the matured balance as pending while the same
        // units are reported as in flight would show a miner its money twice.
        let mut p = a_pool();
        for _ in 0..3 {
            p.pplns.record(W_A);
        }
        p.pplns.record(W_B);
        // 100 units matured, of which 40 are already inside a submitted payout.
        p.matured = Some(Matured {
            units: 100,
            at: 1_500,
        });
        p.matured_current = true;
        p.payout_records
            .push(a_payout("tx1", 1_400, true, &[(W_A, 30), (W_B, 10)]));
        p.rebuild_inflight();
        assert_eq!(p.inflight_units, 40);

        let a = p.earnings_of(W_A);
        let b = p.earnings_of(W_B);
        // Pool-wide pending is 100 - 40 = 60, split 3:1 by shares.
        assert_eq!(a.pool_pending_units, Some(60));
        assert_eq!(worker_pending_units(60, a.shares, a.window_shares), 45);
        assert_eq!(worker_pending_units(60, b.shares, b.window_shares), 15);
        assert_eq!((a.shares, a.inflight_units), (3, 30));
        assert_eq!((b.shares, b.inflight_units), (1, 10));
        // Everything the pool holds is accounted for exactly once: 45 + 15
        // pending, 30 + 10 in flight, 0 paid, and that is the whole 100.
        assert_eq!(45 + 15 + 30 + 10, 100);

        // Confirming the payout moves 40 from in flight to paid and takes it out
        // of the balance; pending is unchanged, because it never contained it.
        let mut guard = p;
        let pp = &mut guard;
        confirm_payout(&mut pp.payout_records, &mut pp.paid, "tx1", 1_600).expect("credited");
        pp.rebuild_inflight();
        pp.matured = Some(Matured {
            units: 60,
            at: 1_650,
        });
        let a = pp.earnings_of(W_A);
        assert_eq!(a.inflight_units, 0);
        assert_eq!(a.paid.units, 30);
        assert_eq!(a.pool_pending_units, Some(60));
        assert_eq!(worker_pending_units(60, a.shares, a.window_shares), 45);
    }

    #[test]
    fn the_pool_can_tell_a_worker_it_has_never_seen_from_one_it_owes_nothing() {
        let mut p = a_pool();
        // Never heard of: no shares, no payments, no work handed out.
        assert!(!p.earnings_of(W_A).known);
        // A worker that fetched work but has not found a share yet IS known: the
        // pool is tracking it, it simply has nothing yet.
        p.extranonce_for(W_A);
        let e = p.earnings_of(W_A);
        assert!(e.known);
        assert_eq!((e.shares, e.paid.units, e.inflight_units), (0, 0, 0));
        // A worker whose shares have all been evicted from the window is still
        // known while it has a paid history: its money did not stop existing.
        let mut q = a_pool();
        q.payout_records
            .push(a_payout("tx1", 1_100, true, &[(W_B, 12)]));
        q.rebuild_inflight();
        confirm_payout(&mut q.payout_records, &mut q.paid, "tx1", 1_200);
        let e = q.earnings_of(W_B);
        assert!(e.known);
        assert_eq!(e.shares, 0);
        assert_eq!(e.paid.units, 12);
        assert_eq!(e.paid.last_hash, "tx1");
    }

    /* ---- the terms the pool advertises are the terms it applies ---- */

    #[test]
    fn the_advertised_terms_are_the_terms_the_settlement_applies() {
        let t = terms_json(
            PPLNS_WINDOW as u64,
            24,
            24,
            0x2000_0000,
            300,
            COINBASE_MATURITY_DEPTH,
            PAYOUT_MATURITY_DEPTH,
        );
        // It is PPLNS, and it says so without leaving room to read PROP into it.
        assert_eq!(t["scheme"].as_str(), Some("PPLNS"));
        assert!(t["scheme_note"].as_str().unwrap().contains("NOT PROP"), "{t}");
        assert_eq!(t["window_shares"].as_u64(), Some(PPLNS_WINDOW as u64));

        // The advertised fee is the fee the split applies.
        let fee = t["fee"]["units"].as_u64().expect("fee");
        let counts = vec![(W_A.to_string(), 3u64), (W_B.to_string(), 1u64)];
        let split = plan_settlement(100, &counts);
        assert_eq!(
            split.iter().map(|(_, u)| *u).sum::<u64>(),
            100 - fee,
            "everything but the advertised fee reaches the miners"
        );

        // The advertised minimum is the minimum the split enforces.
        let min = t["minimum_payout"]["units"].as_u64().expect("minimum");
        let lopsided = vec![(W_A.to_string(), 10_000u64), (W_B.to_string(), 1u64)];
        for (_, u) in plan_settlement(1_000, &lopsided) {
            assert!(u >= min, "nothing below the advertised minimum is ever paid");
        }

        // Every money figure is the chain's own amount, matching its own units.
        for key in ["fee", "minimum_payout", "fee_reserve"] {
            let units = t[key]["units"].as_u64().expect(key);
            assert_eq!(
                t[key]["amount"].as_str(),
                Some(payout_amount(units).to_fin_string().as_str()),
                "{key}"
            );
        }
        assert_eq!(
            t["network_fee_per_settlement_tx"].as_str(),
            Some(chunk_tx_fee().to_fin_string().as_str())
        );
        // The maturity depths are the ones the code waits for, not a description.
        assert_eq!(
            t["coinbase_maturity_blocks"].as_u64(),
            Some(COINBASE_MATURITY_DEPTH)
        );
        assert_eq!(
            t["payout_confirm_blocks"].as_u64(),
            Some(PAYOUT_MATURITY_DEPTH)
        );
        assert_eq!(t["settle_interval_secs"].as_u64(), Some(300));
        assert_eq!(t["recipients_per_settlement_tx"].as_u64(), Some(PAYOUT_CHUNK as u64));
    }

    /* ---- the share target has to be worth something ---- */

    #[test]
    fn a_share_target_the_live_difficulty_cannot_support_is_refused() {
        // Observed on a live run: one worker took the ENTIRE 4096-share window.
        // `check_share_factor` passed, because the factor the operator typed was
        // fine - but at the difficulty in force the derived share target had
        // saturated, every hash was a valid share, and the number of shares a
        // worker was credited with measured how fast it could submit.
        assert!(check_share_factor(24).is_ok());

        let net = pool_core::network_target_hash(LOWEST_DIFFICULTY);
        let served = pool_core::share_target_hash(LOWEST_DIFFICULTY, 24);
        let achieved = pool_core::achieved_share_factor(&net, &served);
        assert_eq!(achieved, 0, "24 asked for, 0 actually served");
        let e = check_share_target(24, achieved, LOWEST_DIFFICULTY).expect_err("refused");
        assert!(e.contains("too low"), "{e}");
        assert!(e.contains("submit"), "{e}");

        // A difficulty with room to shift keeps the factor the operator chose.
        // (The top byte encodes 255 - leading_zero_bits, so this target has 223
        // leading zeros and 24 significant bits: 24 powers of two to spare.)
        let d = 0x20FF_FFFFu32;
        let net = pool_core::network_target_hash(d);
        let served = pool_core::share_target_hash(d, 24);
        assert_eq!(pool_core::achieved_share_factor(&net, &served), 24);
        assert!(check_share_target(24, 24, d).is_ok());

        // The bound is the SAME one share_bits must clear, applied to what is
        // really served.
        assert!(check_share_target(24, MIN_SHARE_FACTOR, 1).is_ok());
        assert!(check_share_target(24, MIN_SHARE_FACTOR - 1, 1).is_err());
    }

    #[test]
    fn a_same_height_reorg_keeps_already_credited_solutions() {
        // Clearing `seen` on a same-height template swap let an A -> B -> A flap
        // re-admit solutions that were already credited, doubling their PPLNS
        // weight at every honest miner's expense.
        let mut seen: HashSet<(u64, [u8; 32], u32)> = HashSet::new();
        seen.insert((100, [7u8; 32], 42));
        seen.insert((100, [8u8; 32], 43));
        seen.insert((99, [7u8; 32], 1));
        prune_seen(&mut seen, 100); // same height, different prev-hash
        assert!(seen.contains(&(100, [7u8; 32], 42)));
        assert!(seen.contains(&(100, [8u8; 32], 43)));
        assert!(!seen.contains(&(99, [7u8; 32], 1)), "stale heights are pruned");
        // A height advance drops everything: those keys fail the freshness check
        // anyway, so keeping them would only grow memory.
        prune_seen(&mut seen, 101);
        assert!(seen.is_empty());
    }

    #[test]
    fn a_block_is_only_confirmed_once_the_chain_has_buried_it() {
        // Counting a block the moment it merely occupies the tip also stopped us
        // watching it, so a shallow reorg of one of OUR blocks could never be
        // detected and blocks_confirmed over-counted for good.
        let h = 1_000u64;
        assert!(!buried_deep(Some(h), h), "0 blocks stacked on top is not buried");
        assert!(!buried_deep(Some(h + COINBASE_MATURITY_DEPTH - 1), h));
        assert!(buried_deep(Some(h + COINBASE_MATURITY_DEPTH), h));
        // No tip this cycle: keep watching rather than finalize on a guess.
        assert!(!buried_deep(None, h));
    }

    #[test]
    fn share_size_that_would_unbalance_the_payout_window_is_refused() {
        // Shares are credited with equal weight, which is only fair while the
        // whole window is much shorter than one block interval.
        assert!(check_share_factor(24).is_ok()); // documented default
        assert!(check_share_factor(MIN_SHARE_FACTOR).is_ok());
        assert!(check_share_factor(MAX_SHARE_FACTOR).is_ok());
        assert!(check_share_factor(MIN_SHARE_FACTOR - 1).is_err());
        assert!(check_share_factor(0).is_err());
        assert!(check_share_factor(MAX_SHARE_FACTOR + 1).is_err());
    }

    #[test]
    fn a_chunk_the_node_never_took_is_not_reported_as_paid() {
        // Observed: the pool printed "settlement done: 2 recipient(s), 35 units
        // across 1 tx(s)" for a payout the node never held. Nobody was paid and
        // the pool wallet never moved, but the summary said money had gone out.
        // Only what the node actually holds may be counted.
        let mut t = SettleTally::default();
        t.record(ChunkOutcome::Failed, 2, 35);
        assert_eq!(t.recipients, 0, "a rejected chunk pays nobody");
        assert_eq!(t.units, 0, "a rejected chunk moves no money");
        assert_eq!(t.txs, 0, "the tx count is what the node took, not what we tried");
        assert_eq!((t.failed_txs, t.failed_units), (1, 35));
        assert!(!t.all_delivered());
        let s = t.summary();
        assert!(s.contains("0 recipient(s), 0 unit(s) across 0 tx(s)"), "{s}");
        assert!(s.contains("FAILED"), "{s}");
        assert!(s.contains("still owed"), "{s}");
    }

    #[test]
    fn a_mixed_settlement_separates_delivered_from_owed() {
        let mut t = SettleTally::default();
        t.record(ChunkOutcome::Delivered, 190, 400);
        t.record(ChunkOutcome::Failed, 12, 30);
        t.record(ChunkOutcome::Unresolved, 5, 11);
        assert_eq!((t.recipients, t.units, t.txs), (190, 400, 1));
        assert_eq!((t.failed_txs, t.failed_units), (1, 30));
        assert_eq!((t.unresolved_txs, t.unresolved_units), (1, 11));
        assert!(!t.all_delivered());
        let s = t.summary();
        assert!(s.contains("190 recipient(s), 400 unit(s) across 1 tx(s)"), "{s}");
        assert!(s.contains("FAILED"), "{s}");
        assert!(s.contains("UNVERIFIED"), "{s}");
        // Even a fully delivered pass must not claim the money is paid: a
        // transaction in the mempool is paid only once a block includes it.
        let mut ok = SettleTally::default();
        ok.record(ChunkOutcome::Delivered, 2, 35);
        assert!(ok.all_delivered());
        let s = ok.summary();
        assert!(s.contains("NOT paid until they confirm"), "{s}");
        assert!(!s.contains("FAILED"), "{s}");
        assert!(!s.contains("UNVERIFIED"), "{s}");
        // Nothing attempted at all must not print a settlement line.
        assert!(!SettleTally::default().attempted());
    }

    #[test]
    fn an_api_accept_is_not_proof_the_node_holds_the_payout() {
        // /submit/transaction answers ret=0 as soon as the node has validated the
        // transaction and handed it to a background task; the mempool insert
        // result is discarded. So the accept response says nothing, and only the
        // node's own view of the hash resolves it.
        let submit_ok: serde_json::Value = serde_json::from_str(r#"{"ret":0,"hash":"ab"}"#).unwrap();
        assert_eq!(find_u64(&submit_ok, "ret"), Some(0));

        let not_found: serde_json::Value =
            serde_json::from_str(r#"{"ret":1,"err":"transaction not found"}"#).unwrap();
        assert_eq!(admission_of(&not_found), Admission::Missing);

        let in_mempool: serde_json::Value =
            serde_json::from_str(r#"{"ret":0,"pending":true}"#).unwrap();
        assert_eq!(admission_of(&in_mempool), Admission::Held);

        let mined: serde_json::Value = serde_json::from_str(r#"{"ret":0,"confirm":9}"#).unwrap();
        assert_eq!(admission_of(&mined), Admission::Held);

        // No answer from the node is NOT a verdict: the payout may be in flight,
        // so it must stay tracked rather than be re-issued (or counted as paid).
        let offline: serde_json::Value =
            serde_json::from_str(r#"{"http_error":"connection refused"}"#).unwrap();
        assert_eq!(admission_of(&offline), Admission::Unresolved);
        let garbled: serde_json::Value = serde_json::from_str(r#"{"ret":0}"#).unwrap();
        assert_eq!(admission_of(&garbled), Admission::Unresolved);
    }

    #[test]
    fn the_share_rate_limiter_never_discards_a_network_block() {
        // At the cap the pool used to answer "busy" before it looked at whether
        // the submission beat the NETWORK target, throwing away a whole block
        // reward to save one entry in a 2-million-entry set.
        assert!(share_limiter_rejects(SEEN_CAP, false), "shares shed at the cap");
        assert!(
            !share_limiter_rejects(SEEN_CAP, true),
            "a network block is never refused by the share limiter"
        );
        assert!(!share_limiter_rejects(SEEN_CAP - 1, false));
        assert!(!share_limiter_rejects(0, false));
        assert!(!share_limiter_rejects(usize::MAX, true));
    }

    /* ---- the pool must mine (and serve) the node's transaction set ---- */

    fn a_template(txs: PackedTxs) -> Template {
        Template {
            height: 4321,
            prevhash: Hash::from([0x5au8; 32]),
            timestamp: 1_700_000_000,
            difficulty: 0x2000_0000,
            target: [0xff; 32],
            coinbase_addr: Address::default(),
            txs: Arc::new(txs),
        }
    }

    #[test]
    fn a_worker_is_served_the_merkle_siblings_it_needs() {
        // Regression: the pool hard-coded `"mkrl_modify_list": []`. That was only
        // safe while its blocks were coinbase-only. A standard worker rebuilds
        // the merkle root from its OWN coinbase nonce folded through this list,
        // so serving an empty list for a block that carries transactions makes
        // every share that worker finds hash to something the pool never
        // reconstructs, and be rejected wholesale.
        let siblings = vec![Hash::from([0x11u8; 32]), Hash::from([0x22u8; 32])];
        let tpl = a_template(PackedTxs {
            bodies: vec![vec![0xaa], vec![0xbb], vec![0xcc]],
            mrklrts: siblings.clone(),
        });
        let body: serde_json::Value =
            serde_json::from_str(&pending_cache_json(&tpl, &[0x7f; 32])).expect("json");
        let served: Vec<String> = body["mkrl_modify_list"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|v| v.as_str().expect("hex string").to_string())
            .collect();
        assert_eq!(
            served,
            siblings.iter().map(|h| hex::encode(h.serialize())).collect::<Vec<_>>(),
            "the worker must get the same siblings the pool folds through"
        );
        assert_eq!(body["height"].as_u64(), Some(4321));
        // The header the worker is handed must already claim all four
        // transactions, or the node rejects the block it eventually builds.
        let intro = hex::decode(body["block_intro"].as_str().expect("intro")).expect("hex");
        assert_eq!(intro.len(), 89);
        assert_eq!(
            u32::from_be_bytes(intro[75..79].try_into().unwrap()),
            4,
            "coinbase plus the node's three transactions"
        );
        // A template with nothing packed still serves an empty list, exactly as
        // before, so the coinbase-only fallback is unchanged for workers.
        let bare: serde_json::Value =
            serde_json::from_str(&pending_cache_json(&a_template(PackedTxs::default()), &[0x7f; 32]))
                .expect("json");
        assert_eq!(bare["mkrl_modify_list"].as_array().map(|a| a.len()), Some(0));
    }

    #[test]
    fn a_worker_whose_every_share_is_rejected_is_shouted_about() {
        // The pool rebuilds each share's header itself, so a worker that computes
        // a different merkle root is REJECTED, never credited - it cannot steal.
        // But it also cannot earn, and a silent reject counter is how a miner
        // burns a day of hashrate for nothing.
        let mut streaks: HashMap<String, u64> = HashMap::new();
        for _ in 1..BAD_STREAK_WARN {
            assert_eq!(bump_bad_streak(&mut streaks, "miner-a"), None);
        }
        assert_eq!(bump_bad_streak(&mut streaks, "miner-a"), Some(BAD_STREAK_WARN));
        // It keeps saying so, but only at each further multiple.
        for _ in 1..BAD_STREAK_WARN {
            assert_eq!(bump_bad_streak(&mut streaks, "miner-a"), None);
        }
        assert_eq!(
            bump_bad_streak(&mut streaks, "miner-a"),
            Some(BAD_STREAK_WARN * 2)
        );
        // One accepted share means the two agree again: the streak restarts.
        streaks.remove("miner-a");
        assert_eq!(bump_bad_streak(&mut streaks, "miner-a"), None);
        // Streaks are per worker, so one broken miner never accuses another.
        assert_eq!(bump_bad_streak(&mut streaks, "miner-b"), None);
        // Bounded: a flood of invented ids must not grow memory without limit.
        let mut flood: HashMap<String, u64> = (0..BAD_STREAK_WORKERS)
            .map(|i| (format!("w{i}"), 1u64))
            .collect();
        assert_eq!(bump_bad_streak(&mut flood, "one-too-many"), None);
        assert_eq!(flood.len(), BAD_STREAK_WORKERS);
        // A worker already tracked still counts once the map is full.
        assert!(flood.contains_key("w0"));
        bump_bad_streak(&mut flood, "w0");
        assert_eq!(flood["w0"], 2);
    }

    #[test]
    fn a_block_the_chain_never_reaches_is_reported_once() {
        // /submit/block validates asynchronously and answers before the verdict,
        // so a refused block leaves no trace but a tip that never gets there. On
        // a chain where this pool is the only miner that is total silence.
        let mut state: HashMap<(u64, [u8; 32]), u32> = HashMap::new();
        let ours = (100u64, [7u8; 32]);
        let pending = [ours];
        // No tip this cycle (node unreachable): never accuse it.
        for _ in 0..BLOCK_STALL_CYCLES * 2 {
            assert!(note_block_stalls(&mut state, &pending, None).is_empty());
        }
        assert!(state.is_empty());
        for _ in 1..BLOCK_STALL_CYCLES {
            assert!(note_block_stalls(&mut state, &pending, Some(99)).is_empty());
        }
        assert_eq!(
            note_block_stalls(&mut state, &pending, Some(99)),
            vec![ours],
            "the chain has not reached our height for far too long"
        );
        // Exactly once: it must not repeat every two seconds afterwards.
        for _ in 0..BLOCK_STALL_CYCLES {
            assert!(note_block_stalls(&mut state, &pending, Some(99)).is_empty());
        }
        // The chain reaching that height clears it, whatever block landed there
        // (accepted, or orphaned - the confirm tally reports that separately).
        assert!(note_block_stalls(&mut state, &pending, Some(100)).is_empty());
        assert!(state.is_empty());
        // A block the pool stopped tracking is dropped rather than leaked.
        note_block_stalls(&mut state, &pending, Some(99));
        assert_eq!(state.len(), 1);
        note_block_stalls(&mut state, &[], Some(99));
        assert!(state.is_empty());
    }

    #[test]
    fn a_refused_block_submission_is_recognised_as_a_refusal() {
        // A refusal costs a whole block reward, and it used to reach nobody but
        // the winning worker's JSON response.
        assert!(!block_submit_refused(r#"{"ret":0,"ok":true}"#));
        assert!(block_submit_refused(r#"{"ret":1,"err":"block parse failed"}"#));
        assert!(block_submit_refused("http_error: connection refused"));
        assert!(block_submit_refused("<html>502 Bad Gateway</html>"));
        assert!(block_submit_refused(""));
    }

    #[test]
    fn the_empty_block_warning_repeats_without_flooding() {
        // The template loop runs every couple of seconds. The operator has to be
        // told, and has to keep being told, without the warning becoming the
        // entire log.
        let mut state: Option<(String, Instant)> = None;
        let t0 = Instant::now();
        assert!(should_warn_now(&mut state, "miner not enabled", t0));
        assert!(!should_warn_now(&mut state, "miner not enabled", t0));
        assert!(
            !should_warn_now(&mut state, "miner not enabled", t0 + TX_WARN_REPEAT - Duration::from_secs(1))
        );
        assert!(should_warn_now(&mut state, "miner not enabled", t0 + TX_WARN_REPEAT));
        // A different reason is news, whenever it arrives.
        assert!(should_warn_now(&mut state, "packing another height", t0 + TX_WARN_REPEAT));
        assert!(!should_warn_now(&mut state, "packing another height", t0 + TX_WARN_REPEAT));
    }

    #[test]
    fn immature_block_income_is_not_distributable() {
        // Two found blocks are still shallow, so their subsidy must stay out of
        // the payout even though the node already credited it to the wallet.
        let immature = [
            (900u64, [1u8; 32], block_reward_units(900)),
            (901u64, [2u8; 32], block_reward_units(901)),
        ];
        let held: u64 = immature.iter().map(|(_, _, u)| *u).sum();
        assert!(held > 0);
        let reserve = 5u64;
        // Balance is exactly the two fresh subsidies plus the reserve: nothing
        // has matured, so the pool must pay nothing at all.
        assert_eq!(distributable_units(held + reserve, held, reserve), None);
        // Once one of them buries, only that block's income is released.
        let matured_one = immature[0].2;
        assert_eq!(
            distributable_units(held + reserve, held - matured_one, reserve),
            Some(matured_one)
        );
    }
}
