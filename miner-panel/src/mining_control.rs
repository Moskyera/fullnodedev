//! Start/stop mining and worker process spawn.

// Only the piped-log path uses these, and it is not compiled on Windows.
#[cfg(not(windows))]
use std::io::{BufRead, BufReader, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
// Only the piped path needs Stdio, and that path is not compiled on Windows,
// where every child gets its own console instead.
#[cfg(not(windows))]
use std::process::Stdio;
#[cfg(not(windows))]
use std::sync::mpsc::SyncSender as Sender;
use std::sync::mpsc::{self, Receiver};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use app::efficiency::MiningStatsSnapshot;

use crate::mining_kind::MiningKind;
use crate::platform;
use crate::{MinerApp, OpenClAction};

const FULLNODE_START_TIMEOUT: Duration = Duration::from_secs(120);
const RPC_PROBE_INITIAL_DELAY: Duration = Duration::from_millis(250);
const RPC_PROBE_MAX_DELAY: Duration = Duration::from_secs(1);
const CHILD_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Where a worker writes its rolling log.
///
/// The worker resolves its config from its own executable directory, and
/// `app::worker_log` puts the log beside that config under the worker's name, so
/// deriving both from the binary path is what makes reader and writer agree.
fn worker_log_path(worker_path: &Path) -> Option<PathBuf> {
    let dir = worker_path.parent()?;
    let stem = worker_path.file_stem()?.to_str()?;
    Some(app::worker_log::log_path(dir, stem))
}

pub(super) fn clear_worker_stats(stats: &mut MiningStatsSnapshot, stats_path: &Path) {
    *stats = MiningStatsSnapshot::default();
    let _ = std::fs::remove_file(stats_path);
}

enum FullnodeSetupResult {
    Ready,
    Waiting {
        fullnode_child: Option<Child>,
        log_rx: Option<Receiver<String>>,
    },
    MissingBinary(PathBuf),
    Failed(String),
}

/// How far a solo start has got. The three conditions are separate questions
/// with separate answers, and each was learned from a start that went wrong:
/// the node was not up, or it was up but years behind, or it was caught up but
/// still inside its own refusal window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartPhase {
    /// Waiting for the node process to answer its RPC port at all.
    NodeUp,
    /// The RPC answers. From here this is no longer a start that can time out:
    /// the node is downloading the chain, which legitimately takes an hour or
    /// more, so the 120 second deadline must stop applying or it would abandon a
    /// node that is working perfectly well.
    Syncing,
    /// Caught up, waiting for the node to actually serve mining work.
    WorkGate,
}

pub(super) struct PendingStart {
    pub worker_path: PathBuf,
    pub deadline: Instant,
    /// Only set when this panel launched the node; lets us report early exits.
    pub fullnode_child: Option<Child>,
    setup_rx: Option<Receiver<FullnodeSetupResult>>,
    readiness_rx: Option<Receiver<bool>>,
    next_probe: Instant,
    probe_delay: Duration,
    cancel_flag: Arc<AtomicBool>,
    phase: StartPhase,
    /// In flight sync probe. One at a time: `probe` does three blocking HTTP
    /// requests and the UI thread must never wait on them.
    sync_rx: Option<Receiver<Option<crate::node_sync::SyncStatus>>>,
    next_sync_probe: Instant,
    /// In flight work probe, and when to give up waiting for work.
    work_rx: Option<Receiver<crate::node_sync::WorkReadiness>>,
    next_work_probe: Instant,
    work_deadline: Instant,
}

/// How often to ask the node how far it has got. The node syncs in batches of
/// 10,000 blocks, so a faster poll only produces a bar that sits still between
/// jumps; this is often enough to feel live and rare enough to stay out of the
/// way of the node's own work.
const SYNC_PROBE_INTERVAL: Duration = Duration::from_secs(3);

/// How often to ask the node whether it will serve work yet.
const WORK_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// How long to wait for the node to start serving work before starting the
/// worker anyway. The node's own refusal window is 30 seconds from ITS launch,
/// which has already partly elapsed by the time the chain is caught up, so this
/// is generous. It exists because the gate is here to remove a spurious error
/// line, not to become a new way for a start to hang: past it the worker starts
/// and prints whatever the node really says.
const WORK_GATE_TIMEOUT: Duration = Duration::from_secs(45);

impl MinerApp {
    pub(super) fn start_mining(&mut self) {
        if self.mining
            || self.pending_start.is_some()
            || self.restart_worker.is_some()
            || self.worker_stopping()
            || self.worker_stop_needs_restart()
            || self.benchmark_operation_active()
            || self.opencl_probe_active()
        {
            return;
        }

        // All-in-one: start local public pool before mining when hosting is enabled.
        if self.mining_kind == MiningKind::Hac
            && self.public_pool.host_enabled
            && !self.public_pool_running
        {
            self.start_public_pool();
            if !self.public_pool_running {
                return;
            }
        }

        self.restart_worker = None;
        self.restart_attempts = 0;
        if self.mining_kind == MiningKind::Hac && self.gpu_presets[self.gpu_idx].slug != "none" {
            self.request_opencl_probe(OpenClAction::StartMining);
            return;
        }
        self.start_mining_after_opencl();
    }

    pub(super) fn start_mining_after_opencl(&mut self) {
        let t = self.t();
        if self.mining_kind == MiningKind::Hacd {
            if self.cpu_presets[self.cpu_idx].supervene == 0 {
                self.status_msg = "HACD is CPU-only; select at least one CPU thread.".into();
                return;
            }
        } else if self.gpu_presets[self.gpu_idx].slug == "none"
            && self.cpu_presets[self.cpu_idx].supervene == 0
        {
            self.status_msg = "Select an OpenCL GPU or enable CPU mining.".into();
            return;
        }

        let worker_path = match self.mining_kind {
            MiningKind::Hac => self.poworker_path.clone(),
            MiningKind::Hacd => self.diaworker_path.clone(),
        };
        let not_found = match self.mining_kind {
            MiningKind::Hac => t.poworker_not_found,
            MiningKind::Hacd => t.diaworker_not_found,
        };
        if !worker_path.exists() {
            self.status_msg = format!(
                "{}
{}",
                not_found,
                worker_path.display()
            );
            return;
        }
        if !self.save_config() {
            return;
        }

        if self.connect_mode == crate::connect::ConnectMode::Solo {
            self.begin_solo_start(worker_path);
        } else {
            self.launch_worker(worker_path);
        }
    }

    fn begin_solo_start(&mut self, worker_path: PathBuf) {
        let work_dir = self.work_dir.clone();
        let connect = self.connect.clone();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel_flag);
        let (tx, rx) = mpsc::channel();
        let spawn_result = thread::Builder::new()
            .name("hacash-fullnode-start".to_string())
            .spawn(move || {
                if rpc_reachable(&connect) {
                    let _ = tx.send(FullnodeSetupResult::Ready);
                    return;
                }
                if thread_cancel.load(Ordering::Acquire) {
                    return;
                }
                let hacash = platform::find_fullnode(&work_dir);
                if !hacash.exists() {
                    let _ = tx.send(FullnodeSetupResult::MissingBinary(hacash));
                    return;
                }
                let result = if platform::fullnode_process_running() {
                    FullnodeSetupResult::Waiting {
                        fullnode_child: None,
                        log_rx: None,
                    }
                } else if thread_cancel.load(Ordering::Acquire) {
                    return;
                } else {
                    let mut cmd = Command::new(&hacash);
                    cmd.current_dir(&work_dir);
                    match MinerApp::spawn_worker_with_logs(&mut cmd) {
                        Ok((child, log_rx)) => FullnodeSetupResult::Waiting {
                            fullnode_child: Some(child),
                            log_rx: Some(log_rx),
                        },
                        Err(error) => FullnodeSetupResult::Failed(error),
                    }
                };
                if thread_cancel.load(Ordering::Acquire) {
                    if let FullnodeSetupResult::Waiting {
                        fullnode_child: Some(child),
                        ..
                    } = result
                    {
                        let _ = queue_child_termination(child);
                    }
                    return;
                }
                let _ = tx.send(result);
            });
        match spawn_result {
            Ok(_) => {
                self.status_msg = self.t().fullnode_starting.to_string();
                self.pending_start = Some(PendingStart {
                    worker_path,
                    deadline: Instant::now() + FULLNODE_START_TIMEOUT,
                    fullnode_child: None,
                    setup_rx: Some(rx),
                    readiness_rx: None,
                    next_probe: Instant::now(),
                    probe_delay: RPC_PROBE_INITIAL_DELAY,
                    cancel_flag,
                    phase: StartPhase::NodeUp,
                    sync_rx: None,
                    next_sync_probe: Instant::now(),
                    work_rx: None,
                    next_work_probe: Instant::now(),
                    work_deadline: Instant::now(),
                });
            }
            Err(error) => {
                self.status_msg = format!("Could not start the full-node check: {error}");
            }
        }
    }

    pub(super) fn launch_worker(&mut self, worker_path: PathBuf) {
        let t = self.t();
        // Start tailing the worker's rolling log BEFORE the spawn. The tail
        // begins at the length the file has right now, so the previous run's
        // lines are never replayed as if they had just been printed, and no
        // line this run writes is missed.
        match worker_log_path(&worker_path) {
            Some(path) => crate::worker_log_tail::start(&path),
            None => crate::worker_log_tail::stop(),
        }
        let mut cmd = Command::new(&worker_path);
        cmd.current_dir(&self.work_dir);
        match Self::spawn_worker_with_logs(&mut cmd) {
            Ok((child, rx)) => {
                self.log_rx = Some(rx);
                clear_worker_stats(&mut self.stats, &self.stats_path);
                self.last_worker_log.clear();
                self.stats_next_read = Instant::now();
                self.worker_started_at = Some(Instant::now());
                self.child = Some(child);
                self.mining = true;
                self.pending_start = None;
                self.status_msg = t.mining_active.to_string();
            }
            Err(e) => self.status_msg = format!("{} {e}", t.start_failed_prefix),
        }
    }

    pub(super) fn poll_pending_start(&mut self) {
        let Some(mut pending) = self.pending_start.take() else {
            return;
        };
        let t = self.t();
        let now = Instant::now();
        // The deadline governs GETTING the node up, not getting it caught up.
        // Once it answers, waiting is the correct behaviour and may last hours.
        if pending.phase == StartPhase::NodeUp && now >= pending.deadline {
            pending.cancel_flag.store(true, Ordering::Release);
            self.fullnode_log_rx = None;
            self.status_msg = format!("{} {}", t.fullnode_not_ready, self.connect);
            return;
        }

        // A node this panel launched can exit at any point, including hours into
        // a chain download, so its log and its exit code are read in every phase
        // rather than only while waiting for the RPC to come up.
        if let Some(rx) = &self.fullnode_log_rx {
            while let Ok(line) = rx.try_recv() {
                if !line.trim().is_empty() {
                    self.last_worker_log = line;
                }
            }
        }
        if let Some(child) = &mut pending.fullnode_child {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    self.fullnode_log_rx = None;
                    self.sync_status = None;
                    self.status_msg = if self.last_worker_log.is_empty() {
                        format!("{} {exit}", t.fullnode_not_ready)
                    } else {
                        format!("{} {exit}: {}", t.fullnode_not_ready, self.last_worker_log)
                    };
                    return;
                }
                Err(e) => {
                    self.fullnode_log_rx = None;
                    self.sync_status = None;
                    self.status_msg = format!("{} {e}", t.fullnode_not_ready);
                    return;
                }
                Ok(None) => {}
            }
        }

        match pending.phase {
            StartPhase::Syncing => {
                self.poll_node_sync(pending);
                return;
            }
            StartPhase::WorkGate => {
                self.poll_work_gate(pending);
                return;
            }
            StartPhase::NodeUp => {}
        }

        if let Some(setup_rx) = pending.setup_rx.take() {
            match setup_rx.try_recv() {
                Ok(FullnodeSetupResult::Ready) => {
                    // NOT ready to mine, only ready to talk. Mining on a node
                    // part way through the chain hashes against blocks the
                    // network settled long ago: every solution is rejected, and
                    // because block_hash_repeat is smaller down there the
                    // reported hashrate is HIGHER than normal, so it reads as
                    // success. Wait for the chain before starting the worker.
                    pending.phase = StartPhase::Syncing;
                    pending.next_sync_probe = Instant::now();
                    self.pending_start = Some(pending);
                    return;
                }
                Ok(FullnodeSetupResult::Waiting {
                    fullnode_child,
                    log_rx,
                }) => {
                    pending.fullnode_child = fullnode_child;
                    self.fullnode_log_rx = log_rx;
                    pending.next_probe = now;
                }
                Ok(FullnodeSetupResult::MissingBinary(path)) => {
                    self.status_msg = format!("{}\n{}", t.fullnode_exe_not_found, path.display());
                    return;
                }
                Ok(FullnodeSetupResult::Failed(error)) => {
                    self.status_msg = format!("{} {error}", t.start_failed_prefix);
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    pending.setup_rx = Some(setup_rx);
                    self.pending_start = Some(pending);
                    return;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.status_msg = "The full-node startup check stopped unexpectedly.".into();
                    return;
                }
            }
        }

        if let Some(readiness_rx) = pending.readiness_rx.take() {
            match readiness_rx.try_recv() {
                Ok(true) => {
                    // The RPC answering means the node is alive, nothing more.
                    // A node this panel just started is the LIKELIEST one to be
                    // mid-download, so it goes through the same chain gate as a
                    // node that was already running; that gate used to be
                    // skipped on exactly this path.
                    pending.phase = StartPhase::Syncing;
                    pending.next_sync_probe = now;
                    self.pending_start = Some(pending);
                    return;
                }
                Ok(false) | Err(mpsc::TryRecvError::Disconnected) => {
                    pending.next_probe = now + pending.probe_delay;
                    pending.probe_delay = pending
                        .probe_delay
                        .saturating_mul(2)
                        .min(RPC_PROBE_MAX_DELAY);
                }
                Err(mpsc::TryRecvError::Empty) => {
                    pending.readiness_rx = Some(readiness_rx);
                }
            }
        }

        if pending.readiness_rx.is_none() && now >= pending.next_probe {
            let connect = self.connect.clone();
            let (tx, rx) = mpsc::channel();
            match thread::Builder::new()
                .name("hacash-rpc-ready".to_string())
                .spawn(move || {
                    let _ = tx.send(rpc_reachable(&connect));
                }) {
                Ok(_) => pending.readiness_rx = Some(rx),
                Err(error) => {
                    pending.next_probe = now + RPC_PROBE_MAX_DELAY;
                    self.status_msg = format!("Full-node readiness check failed: {error}");
                }
            }
        }
        self.pending_start = Some(pending);
    }

    pub(super) fn stop_mining(&mut self) {
        self.cancel_opencl_probe();
        if let Some(pending) = self.pending_start.take() {
            pending.cancel_flag.store(true, Ordering::Release);
        }
        let stop_rx = self.child.take().map(queue_child_termination);
        self.mining = false;
        self.restart_worker = None;
        self.restart_attempts = 0;
        self.worker_started_at = None;
        self.log_rx = None;
        self.fullnode_log_rx = None;
        crate::worker_log_tail::stop();
        clear_worker_stats(&mut self.stats, &self.stats_path);
        self.last_worker_log.clear();
        if let Some(rx) = stop_rx {
            self.worker_stop_rx = Some(rx);
            self.worker_stop_failed = false;
            self.status_msg = "Stopping miner safely...".to_string();
        } else if self.worker_stopping() {
            self.status_msg = "Stopping miner safely...".to_string();
        } else if self.worker_stop_needs_restart() {
            self.status_msg =
                "Worker stop could not be confirmed. End the worker process, then restart the panel."
                    .to_string();
        } else {
            self.status_msg = self.t().mining_stopped.to_string();
        }
    }

    pub(super) fn stop_mining_on_exit(&mut self) {
        self.cancel_opencl_probe();
        if let Some(pending) = self.pending_start.take() {
            pending.cancel_flag.store(true, Ordering::Release);
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
        }
        self.mining = false;
        self.restart_worker = None;
        self.worker_started_at = None;
        self.log_rx = None;
        self.fullnode_log_rx = None;
        crate::worker_log_tail::stop();
    }

    pub(super) fn spawn_worker_with_logs(
        cmd: &mut Command,
    ) -> Result<(Child, Receiver<String>), String> {
        // On Windows the node and the workers get their OWN console window, so
        // the operator can watch them. That is not decoration: a node
        // downloading the chain prints every batch it inserts, and hiding that
        // is how a stalled sync came to look identical to a healthy one from
        // this panel.
        //
        // A child with its own console is not piping anything back, so the
        // channel returned here stays empty. That is why the workers also write
        // every line they print to a rolling log beside their config
        // (`app::worker_log`), which `worker_log_tail` reads: the operator gets
        // the console window AND the panel gets the lines. Status was never
        // affected either way, since it comes from the RPC and the exit code.
        //
        // The node is not covered by that log. Its output comes from the node,
        // chain, server and mint crates, none of which can depend on `app`, so
        // for the node the console window really is the only copy.
        #[cfg(windows)]
        {
            platform::configure_visible_command(cmd);
            let child = cmd.spawn().map_err(|e| e.to_string())?;
            let (_tx, rx) = mpsc::sync_channel(LOG_CHANNEL_CAP);
            return Ok((child, rx));
        }
        // Elsewhere a GUI-launched child has no terminal to attach to, so keep
        // piping and let the panel show the log itself.
        #[cfg(not(windows))]
        {
            platform::configure_background_command(cmd);
            let mut child = cmd
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())?;
            // Bounded so a hung UI cannot grow worker log memory without limit.
            let (tx, rx) = mpsc::sync_channel(LOG_CHANNEL_CAP);
            if let Some(out) = child.stdout.take() {
                spawn_log_drainer(out, tx.clone());
            }
            if let Some(err) = child.stderr.take() {
                spawn_log_drainer(err, tx);
            }
            Ok((child, rx))
        }
    }
}

/// Kill and reap outside egui. The receiver resolves only after `try_wait`
/// confirms exit, or with a bounded timeout/error; no caller needs to block.
pub(super) fn queue_child_termination(mut child: Child) -> Receiver<Result<(), String>> {
    // `kill` is a prompt OS signal; perform it before handing the handle to a
    // thread so even immediate app shutdown cannot orphan a live miner.
    let kill_error = child.kill().err().map(|error| error.to_string());
    let (tx, rx) = mpsc::channel();
    let thread_tx = tx.clone();
    let spawn_result = thread::Builder::new()
        .name("hacash-child-reaper".to_string())
        .spawn(move || {
            if let Some(error) = kill_error {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        let _ = thread_tx.send(Ok(()));
                    }
                    _ => {
                        let _ = thread_tx.send(Err(format!("could not stop worker: {error}")));
                    }
                }
                return;
            }
            let deadline = Instant::now() + CHILD_REAP_TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        let _ = thread_tx.send(Ok(()));
                        return;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Ok(None) => {
                        let _ = thread_tx.send(Err("worker did not stop within 2 seconds".into()));
                        return;
                    }
                    Err(error) => {
                        let _ = thread_tx.send(Err(format!("worker reap failed: {error}")));
                        return;
                    }
                }
            }
        });
    if let Err(error) = spawn_result {
        let _ = tx.send(Err(format!("could not start worker reaper: {error}")));
    }
    rx
}

/// Max queued log lines from worker stdout/stderr (oldest dropped when full).
const LOG_CHANNEL_CAP: usize = 512;

/// Drains a child's piped output into the panel. Not built on Windows: children
/// there own a console and pipe nothing back.
#[cfg(not(windows))]
fn spawn_log_drainer<R: Read + Send + 'static>(stream: R, tx: Sender<String>) {
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
            match tx.try_send(line) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    // Drop when the UI is not draining fast enough.
                }
                Err(mpsc::TrySendError::Disconnected(_)) => break,
            }
        }
    });
}

impl MinerApp {
    /// Wait for the node to catch up, showing how far it has got, and start the
    /// worker only when it has.
    ///
    /// Never blocks the UI: the probe does three HTTP requests, so it runs on its
    /// own thread and this reads the answer when it arrives.
    pub(super) fn poll_node_sync(&mut self, mut pending: PendingStart) {
        let now = Instant::now();

        if let Some(rx) = pending.sync_rx.take() {
            match rx.try_recv() {
                Ok(Some(status)) => {
                    let synced = status.is_synced();
                    self.sync_status = Some(status);
                    pending.next_sync_probe = now + SYNC_PROBE_INTERVAL;
                    if synced {
                        self.sync_status = None;
                        self.enter_work_gate(pending);
                        return;
                    }
                }
                // Could not tell. Not an error and not a reason to start mining:
                // a node mid-batch can be briefly unresponsive, and treating
                // silence as "caught up" is the whole bug being fixed here.
                Ok(None) => pending.next_sync_probe = now + SYNC_PROBE_INTERVAL,
                Err(std::sync::mpsc::TryRecvError::Empty) => pending.sync_rx = Some(rx),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    pending.next_sync_probe = now + SYNC_PROBE_INTERVAL
                }
            }
        }

        if pending.sync_rx.is_none() && now >= pending.next_sync_probe {
            let connect = self.connect.clone();
            let (tx, rx) = mpsc::channel();
            if std::thread::Builder::new()
                .name("hacash-sync-probe".to_string())
                .spawn(move || {
                    let _ = tx.send(crate::node_sync::probe(&connect));
                })
                .is_ok()
            {
                pending.sync_rx = Some(rx);
            } else {
                pending.next_sync_probe = now + SYNC_PROBE_INTERVAL;
            }
        }

        self.pending_start = Some(pending);
    }

    /// The chain is caught up. Last question before the worker starts: will the
    /// node serve it any work?
    fn enter_work_gate(&mut self, mut pending: PendingStart) {
        let now = Instant::now();
        pending.phase = StartPhase::WorkGate;
        pending.next_work_probe = now;
        pending.work_deadline = now + WORK_GATE_TIMEOUT;
        self.status_msg = self.t().node_warmup.to_string();
        self.poll_work_gate(pending);
    }

    /// Hold the worker back until the node stops refusing work.
    ///
    /// The node rejects `/query/miner/pending` for the first 30 seconds after
    /// ITS start, and the panel used to spawn the worker inside that window, so
    /// every single solo start put a red `Error: get block stuff error` line in
    /// the worker log. It recovered on its own, which is the problem: it taught
    /// the operator to read past red lines.
    ///
    /// The wait is on the node's answer, not on a 30 second sleep, because the
    /// window is counted from the node's launch and the node is usually already
    /// past it by the time the chain check finishes, or was started by someone
    /// else long ago and never had one.
    fn poll_work_gate(&mut self, mut pending: PendingStart) {
        use crate::node_sync::WorkReadiness;
        let now = Instant::now();

        if let Some(rx) = pending.work_rx.take() {
            match rx.try_recv() {
                Ok(WorkReadiness::Serving) => {
                    self.launch_worker(pending.worker_path);
                    return;
                }
                // Still refusing, or could not tell. Neither starts the worker.
                Ok(WorkReadiness::StartupWindow) | Ok(WorkReadiness::Unknown) => {
                    pending.next_work_probe = now + WORK_PROBE_INTERVAL;
                }
                Err(mpsc::TryRecvError::Empty) => pending.work_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {
                    pending.next_work_probe = now + WORK_PROBE_INTERVAL;
                }
            }
        }

        // Never a way to hang. A node that keeps refusing past the deadline is
        // no longer inside a startup window, and whatever it says next belongs
        // in the worker's log where the operator can read it.
        if now >= pending.work_deadline {
            self.launch_worker(pending.worker_path);
            return;
        }

        if pending.work_rx.is_none() && now >= pending.next_work_probe {
            let connect = self.connect.clone();
            let (tx, rx) = mpsc::channel();
            if thread::Builder::new()
                .name("hacash-work-probe".to_string())
                .spawn(move || {
                    let _ = tx.send(crate::node_sync::probe_work(&connect));
                })
                .is_ok()
            {
                pending.work_rx = Some(rx);
            } else {
                pending.next_work_probe = now + WORK_PROBE_INTERVAL;
            }
        }

        self.pending_start = Some(pending);
    }
}

pub(super) fn rpc_reachable(connect: &str) -> bool {
    let Ok(addrs) = connect.trim().to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(800)).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    // Stdio is cfg-gated out of the Windows build path, but the reaper test
    // needs it on every target to silence its throwaway child.
    use std::process::Stdio;

    const REAPER_CHILD_ENV: &str = "HACASH_PANEL_REAPER_CHILD";

    #[test]
    fn reaper_child_entrypoint() {
        if std::env::var(REAPER_CHILD_ENV).as_deref() == Ok("slow") {
            thread::sleep(Duration::from_secs(10));
        }
    }

    #[test]
    fn child_termination_returns_to_ui_immediately() {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .arg("--exact")
            .arg("mining_control::tests::reaper_child_entrypoint")
            .arg("--nocapture")
            .env(REAPER_CHILD_ENV, "slow")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        platform::configure_background_command(&mut command);
        let child = command.spawn().expect("spawn slow child");

        let started = Instant::now();
        let completion = queue_child_termination(child);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "queueing termination blocked the caller"
        );
        assert!(
            completion
                .recv_timeout(Duration::from_secs(3))
                .expect("reaper response")
                .is_ok(),
            "child should be killed and reaped within the bounded deadline"
        );
    }

    #[test]
    fn clear_worker_stats_removes_the_stale_snapshot_file() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hacash-panel-stale-stats-{}-{unique}.json",
            std::process::id()
        ));
        std::fs::write(&path, br#"{"status":"mining"}"#).expect("write stale stats");
        let mut stats = MiningStatsSnapshot {
            status: "mining".to_string(),
            updated_unix_ms: 42,
            ..Default::default()
        };

        clear_worker_stats(&mut stats, &path);

        assert!(stats.status.is_empty());
        assert_eq!(stats.updated_unix_ms, 0);
        assert!(!path.exists());
    }
}
