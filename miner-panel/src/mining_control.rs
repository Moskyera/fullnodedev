//! Start/stop mining and worker process spawn.

use std::io::{BufRead, BufReader, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender as Sender};
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
}

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
                });
            }
            Err(error) => {
                self.status_msg = format!("Could not start the full-node check: {error}");
            }
        }
    }

    pub(super) fn launch_worker(&mut self, worker_path: PathBuf) {
        let t = self.t();
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
        if now >= pending.deadline {
            pending.cancel_flag.store(true, Ordering::Release);
            self.fullnode_log_rx = None;
            self.status_msg = format!("{} {}", t.fullnode_not_ready, self.connect);
            return;
        }

        if let Some(setup_rx) = pending.setup_rx.take() {
            match setup_rx.try_recv() {
                Ok(FullnodeSetupResult::Ready) => {
                    self.launch_worker(pending.worker_path);
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
                    self.status_msg = if self.last_worker_log.is_empty() {
                        format!("{} {exit}", t.fullnode_not_ready)
                    } else {
                        format!("{} {exit}: {}", t.fullnode_not_ready, self.last_worker_log)
                    };
                    return;
                }
                Err(e) => {
                    self.fullnode_log_rx = None;
                    self.status_msg = format!("{} {e}", t.fullnode_not_ready);
                    return;
                }
                Ok(None) => {}
            }
        }

        if let Some(readiness_rx) = pending.readiness_rx.take() {
            match readiness_rx.try_recv() {
                Ok(true) => {
                    self.launch_worker(pending.worker_path);
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
    }

    pub(super) fn spawn_worker_with_logs(
        cmd: &mut Command,
    ) -> Result<(Child, Receiver<String>), String> {
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
