//! miner-stats.json reader, worker logs, and bounded automatic recovery.

use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use app::efficiency::MiningStatsSnapshot;

use crate::MinerApp;
use crate::mining_control::clear_worker_stats;
use crate::mining_kind::MiningKind;

const STATS_FILE_POLL_INTERVAL: Duration = Duration::from_millis(350);

impl MinerApp {
    pub(super) fn poll_stats(&mut self) {
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

        self.poll_scheduled_restart();
        if !self.mining {
            return;
        }

        let t = self.t();
        if let Some(rx) = &self.log_rx {
            while let Ok(line) = rx.try_recv() {
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

        let exit_text = match exit {
            Ok(status) => status.to_string(),
            Err(error) => error,
        };
        let detail = if self.last_worker_log.is_empty() {
            exit_text
        } else {
            format!("{exit_text}: {}", self.last_worker_log)
        };

        if self.restart_attempts < 3 {
            self.restart_attempts += 1;
            let delay = Duration::from_secs(3 * self.restart_attempts as u64);
            let worker = match self.mining_kind {
                MiningKind::Hac => self.poworker_path.clone(),
                MiningKind::Hacd => self.diaworker_path.clone(),
            };
            self.restart_worker = Some((worker, Instant::now() + delay));
            self.status_msg = format!(
                "{}: {}. Automatic retry {}/3 in {}s.",
                t.miner_exited,
                detail,
                self.restart_attempts,
                delay.as_secs()
            );
        } else {
            self.restart_worker = None;
            self.status_msg = format!(
                "{}: {}. Automatic recovery stopped after 3 attempts.",
                t.miner_exited, detail
            );
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
            return "—".to_string();
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let sec = now_ms.saturating_sub(ms) / 1000;
        if sec < 60 {
            format!("{sec}s")
        } else if sec < 3600 {
            format!("{}m", sec / 60)
        } else {
            format!("{}h", sec / 3600)
        }
    }
}
