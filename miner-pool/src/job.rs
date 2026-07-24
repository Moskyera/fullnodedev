use std::sync::RwLock;
use std::time::{Duration, Instant};

use serde_json::Value as JV;

/// Latest mining job mirrored from the upstream fullnode.
#[derive(Debug, Clone)]
pub struct MiningJob {
    pub height: u64,
    pub raw: JV,
    pub job_id: String,
    /// When this job was last mirrored from upstream. Used to treat work as
    /// absent once the upstream node stops answering, instead of serving a
    /// frozen height at full hashrate for the whole outage.
    pub received_at: Instant,
}

/// Freshness budget for a mirrored job, derived from the poll interval. `update`
/// runs on every successful poll (even when the height is unchanged), so this
/// only elapses during a real upstream outage, never between blocks.
pub fn job_ttl(poll_ms: u64) -> Duration {
    Duration::from_millis(poll_ms.saturating_mul(4)).max(Duration::from_secs(15))
}

pub struct JobHub {
    inner: RwLock<Option<MiningJob>>,
}

impl JobHub {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(None),
        }
    }

    pub fn update(&self, height: u64, raw: JV) {
        let job_id = format!("h{height}");
        // Poison-tolerant: a poisoned lock must never permanently wedge job
        // refresh on a 24/7 pool. The guarded value is a whole-job replacement,
        // so a recovered inner value is always self-consistent.
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        *g = Some(MiningJob {
            height,
            raw,
            job_id,
            received_at: Instant::now(),
        });
    }

    pub fn current(&self) -> Option<MiningJob> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The current job only while it is younger than `ttl`; None once upstream
    /// has stopped refreshing it, so callers can report an outage rather than
    /// hand out dead work.
    pub fn current_fresh(&self, ttl: Duration) -> Option<MiningJob> {
        self.current().filter(|j| j.received_at.elapsed() <= ttl)
    }

    pub fn height(&self) -> u64 {
        self.current().map(|j| j.height).unwrap_or(0)
    }

    /// Height of a fresh job, or 0 when there is none (absent or stale).
    pub fn height_fresh(&self, ttl: Duration) -> u64 {
        self.current_fresh(ttl).map(|j| j.height).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn job_hub_updates_height_and_id() {
        let hub = JobHub::new();
        assert_eq!(hub.height(), 0);
        hub.update(100, json!({"height": 100, "block_intro": "aa"}));
        let j = hub.current().unwrap();
        assert_eq!(j.height, 100);
        assert_eq!(j.job_id, "h100");
    }

    #[test]
    fn stale_job_is_not_served_as_fresh() {
        let hub = JobHub::new();
        hub.update(100, json!({"height": 100, "block_intro": "aa"}));
        // Fresh under a generous ttl.
        assert!(hub.current_fresh(Duration::from_secs(60)).is_some());
        assert_eq!(hub.height_fresh(Duration::from_secs(60)), 100);
        // Aged out: the last good job must not be handed out as work.
        std::thread::sleep(Duration::from_millis(20));
        assert!(hub.current_fresh(Duration::from_millis(1)).is_none());
        assert_eq!(hub.height_fresh(Duration::from_millis(1)), 0);
        // current()/height() still expose the last known value for reporting.
        assert_eq!(hub.height(), 100);
    }

    #[test]
    fn job_ttl_has_a_floor_and_scales_with_poll_interval() {
        assert_eq!(job_ttl(2000), Duration::from_secs(15));
        assert_eq!(job_ttl(10_000), Duration::from_secs(40));
    }
}
