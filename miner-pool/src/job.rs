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

/// How much of block_intro is folded into the job id to distinguish two templates
/// at the same height. Long enough that a same-height reorg always changes it.
const INTRO_TAG_CHARS: usize = 16;

/// Read the height back out of a job id produced by `JobHub::update`.
///
/// A stratum submit carries only the job id, so this is how a solution is billed to
/// the height it was actually mined for. That matters: the node keeps several recent
/// templates, so a solution found a moment before the tip moved is still accepted at
/// its own height, but only if it is submitted with that height rather than the
/// current one. Accepts the current `h{height}_{tag}` form and the older bare
/// `h{height}` form, and returns None for anything else so the caller can fall back.
pub fn job_height(job_id: &str) -> Option<u64> {
    let rest = job_id.strip_prefix('h')?;
    let digits = match rest.split_once('_') {
        Some((h, _tag)) => h,
        None => rest,
    };
    digits.parse::<u64>().ok()
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
        // Include a short fingerprint of block_intro so same-height reorgs
        // (template replaced without height change) get a new job_id and
        // stratum clients receive mining.notify. Height-only ids left miners
        // grinding an orphaned template after a tip rewrite.
        let intro = raw
            .get("block_intro")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // Char-safe: block_intro comes from the upstream node's JSON, so it is not
        // guaranteed ASCII. Byte slicing it could land mid-codepoint and panic the
        // poll task on data we do not control.
        let skip = intro.chars().count().saturating_sub(INTRO_TAG_CHARS);
        let intro_tag: String = intro.chars().skip(skip).collect();
        // The height must stay parseable out of the id: stratum submits carry only the
        // job_id, and job_height() reads the height back from it so a solution found
        // just before the tip moved is still submitted at the height it was mined for.
        let job_id = format!("h{height}_{intro_tag}");
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
        assert_eq!(j.job_id, "h100_aa");
        // Same height, different intro → new job_id (same-height reorg).
        hub.update(100, json!({"height": 100, "block_intro": "bb"}));
        assert_eq!(hub.current().unwrap().job_id, "h100_bb");
    }

    #[test]
    fn height_survives_a_round_trip_through_the_job_id() {
        // A stratum submit carries only the job id. If the height cannot be read back
        // out, a winner found just before the tip moved gets billed to the CURRENT
        // height and is rejected, losing a real block. Every id this hub emits must
        // round-trip, including the reorg tag and a long real block_intro.
        let hub = JobHub::new();
        for (h, intro) in [
            (100u64, "aa"),
            (1u64, ""),
            (4_294_967_296u64, "00112233445566778899aabbccddeeff0011"),
            (u64::MAX, "ff"),
        ] {
            hub.update(h, json!({"height": h, "block_intro": intro}));
            let id = hub.current().unwrap().job_id;
            assert_eq!(job_height(&id), Some(h), "job_id {id} lost its height");
        }
        // The older bare form still parses, so a client holding a pre-upgrade job id
        // is billed correctly instead of silently falling back to the current height.
        assert_eq!(job_height("h100"), Some(100));
        // Anything unrecognised must say so rather than guess a wrong height.
        assert_eq!(job_height("garbage"), None);
        assert_eq!(job_height("h"), None);
        assert_eq!(job_height("hxx_aa"), None);
    }

    #[test]
    fn a_non_ascii_block_intro_does_not_panic() {
        // block_intro is upstream JSON, not something we control. Byte slicing it to
        // build the tag could land mid-codepoint and take down the poll task.
        let hub = JobHub::new();
        hub.update(7, json!({"height": 7, "block_intro": "ααααααααααααααααββ"}));
        let id = hub.current().unwrap().job_id;
        assert_eq!(job_height(&id), Some(7));
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
