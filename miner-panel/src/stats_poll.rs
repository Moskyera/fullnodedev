//! miner-stats.json reader and worker log polling.

use app::efficiency::MiningStatsSnapshot;

use crate::MinerApp;

impl MinerApp {
    pub(super) fn poll_stats(&mut self) {
        self.poll_pending_start();
        self.poll_benchmark();
        let t = self.t();
        if let Ok(data) = std::fs::read_to_string(&self.stats_path) {
            if let Ok(s) = serde_json::from_str::<MiningStatsSnapshot>(&data) {
                self.stats = s;
            }
        }
        if !self.mining {
            return;
        }
        if let Some(rx) = &self.log_rx {
            while let Ok(line) = rx.try_recv() {
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
        if let Some(child) = &mut self.child {
            if let Ok(Some(_)) = child.try_wait() {
                self.mining = false;
                self.child = None;
                self.status_msg = t.miner_exited.to_string();
            }
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