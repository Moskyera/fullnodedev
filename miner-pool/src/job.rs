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

/// Where `prevhash` sits inside a serialized block intro, in hex characters.
///
/// The intro is a fixed 89 bytes (protocol/src/block/intro.rs): version 1 |
/// height 5 | timestamp 5 | prevhash 32 | mrklroot 32 | tx_count 4 | nonce 4 |
/// difficulty 4 | witness_stage 2. prevhash is therefore bytes 11..43, i.e. hex
/// characters 22..86 of the string the node serves.
const INTRO_PREVHASH_SKIP: usize = 22;
const INTRO_PREVHASH_CHARS: usize = 64;

/// FNV-1a over the tag source. Hashing keeps the tag a fixed 16 hex characters
/// with no '_' in it whatever the upstream sent, so `job_height` always splits the
/// id at the right place. Written out rather than taken from `DefaultHasher` so a
/// given template maps to the same id in every build and every process.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Fingerprint of the template's parent link, so two templates at the same height
/// get different job ids and stratum clients get a `mining.notify`.
///
/// prevhash is the field to key on, and it is the only one. The node repacks a
/// template exactly when its parent stops being the tip
/// (mint/src/api/miner_pending.rs:14), so a second template at an already-served
/// height only ever appears after a same-height reorg, and it always carries a
/// different parent.
///
/// Deliberately NOT the tail of the intro: the last 16 hex characters are bytes
/// 81..89, the low bytes of the template nonce (always 0 in a served template),
/// difficulty (unchanged except at a retarget) and witness_stage, so they are
/// identical across a reorg and discriminate nothing.
///
/// Deliberately NOT the mrklroot either: the node bumps the coinbase nonce on
/// every /query/miner/pending call (mint/src/api/common.rs:232-238), which
/// rewrites the mrklroot in every single reply. Folding that in would change the
/// job id on every poll and push a notify to every client several times a minute,
/// resetting their work for nothing.
///
/// Char-safe throughout: block_intro is upstream JSON and is not guaranteed to be
/// ASCII hex, so it is only ever walked by chars, never byte-sliced.
fn intro_tag(intro: &str) -> String {
    let prevhash: String = intro
        .chars()
        .skip(INTRO_PREVHASH_SKIP)
        .take(INTRO_PREVHASH_CHARS)
        .collect();
    // A short or garbled intro has no prevhash field to read. Fall back to the
    // whole string, which at least still varies, rather than to a constant.
    let src = if prevhash.chars().count() == INTRO_PREVHASH_CHARS {
        prevhash
    } else {
        intro.to_string()
    };
    format!("{:016x}", fnv1a64(src.as_bytes()))
}

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
        // Fingerprint the template's parent so same-height reorgs (template
        // replaced without height change) get a new job_id and stratum clients
        // receive mining.notify. Height-only ids left miners grinding an orphaned
        // template after a tip rewrite, for up to a whole block interval.
        let intro = raw
            .get("block_intro")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // The height must stay parseable out of the id: stratum submits carry only the
        // job_id, and job_height() reads the height back from it so a solution found
        // just before the tip moved is still submitted at the height it was mined for.
        let job_id = format!("h{height}_{}", intro_tag(intro));
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
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
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

    /// A realistic 178-hex-character block intro: version 1 | height 5 |
    /// timestamp 5 | prevhash 32 | mrklroot 32 | tx_count 4 | nonce 4 |
    /// difficulty 4 | witness_stage 2 = 89 bytes. The fields that a served
    /// template actually leaves at zero (nonce, witness_stage) are left at zero
    /// here too, so the fixture matches what the node really sends.
    fn intro_hex(height: u64, timestamp: u64, prevhash: u8, mrklroot: u8) -> String {
        let mut b = vec![0u8; 89];
        b[0] = 1; // version
        b[1..6].copy_from_slice(&height.to_be_bytes()[3..8]); // Uint5 height
        b[6..11].copy_from_slice(&timestamp.to_be_bytes()[3..8]); // Uint5 timestamp
        b[11..43].iter_mut().for_each(|x| *x = prevhash);
        b[43..75].iter_mut().for_each(|x| *x = mrklroot);
        b[75..79].copy_from_slice(&2u32.to_be_bytes()); // tx_count
        b[83..87].copy_from_slice(&0x1d00_ffffu32.to_be_bytes()); // difficulty
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn job_hub_updates_height_and_id() {
        let hub = JobHub::new();
        assert_eq!(hub.height(), 0);
        hub.update(100, json!({"height": 100, "block_intro": "aa"}));
        let j = hub.current().unwrap();
        assert_eq!(j.height, 100);
        assert!(
            j.job_id.starts_with("h100_"),
            "unexpected job_id {}",
            j.job_id
        );
        let first = j.job_id;
        // Same height, different intro -> new job_id (same-height reorg).
        hub.update(100, json!({"height": 100, "block_intro": "bb"}));
        assert_ne!(hub.current().unwrap().job_id, first);
    }

    #[test]
    fn a_same_height_reorg_changes_the_job_id() {
        // The whole point of the tag. Two templates at the same height differ in
        // their parent, and nowhere else that the pool can see. If the id does not
        // change, stratum.rs never pushes mining.notify and every client keeps
        // hashing an orphaned parent until the height moves, which on mainnet is a
        // full block interval of worthless shares.
        let hub = JobHub::new();
        hub.update(
            500,
            json!({"height": 500, "block_intro": intro_hex(500, 1_700_000_000, 0x11, 0xaa)}),
        );
        let before = hub.current().unwrap().job_id;
        hub.update(
            500,
            json!({"height": 500, "block_intro": intro_hex(500, 1_700_000_030, 0x22, 0xbb)}),
        );
        let after = hub.current().unwrap().job_id;
        assert_ne!(before, after, "same-height reorg did not change the job id");
        assert_eq!(job_height(&after), Some(500));
    }

    #[test]
    fn a_new_coinbase_nonce_alone_does_not_change_the_job_id() {
        // The node bumps the coinbase nonce on every /query/miner/pending call, so
        // the mrklroot in block_intro is different in every reply. If that fed the
        // tag, every poll would emit a mining.notify and reset every client's work
        // for nothing. Only the parent link may move the id.
        let hub = JobHub::new();
        hub.update(
            501,
            json!({"height": 501, "block_intro": intro_hex(501, 1_700_000_000, 0x11, 0xaa)}),
        );
        let before = hub.current().unwrap().job_id;
        hub.update(
            501,
            json!({"height": 501, "block_intro": intro_hex(501, 1_700_000_000, 0x11, 0xbb)}),
        );
        assert_eq!(
            hub.current().unwrap().job_id,
            before,
            "a coinbase-nonce bump must not look like new work"
        );
    }

    #[test]
    fn height_survives_a_round_trip_through_the_job_id() {
        // A stratum submit carries only the job id. If the height cannot be read back
        // out, a winner found just before the tip moved gets billed to the CURRENT
        // height and is rejected, losing a real block. Every id this hub emits must
        // round-trip, including the reorg tag and a long real block_intro.
        let hub = JobHub::new();
        for (h, intro) in [
            (100u64, "aa".to_string()),
            (1u64, String::new()),
            (
                4_294_967_296u64,
                "00112233445566778899aabbccddeeff0011".to_string(),
            ),
            (u64::MAX, "ff".to_string()),
            // A real full-length intro, and one with an underscore in it: the tag
            // is hashed to fixed-width hex, so no upstream payload can smuggle a
            // separator into the id and steal the height.
            (900u64, intro_hex(900, 1_700_000_000, 0x11, 0xaa)),
            (901u64, "aa_bb_cc".to_string()),
        ] {
            hub.update(h, json!({"height": h, "block_intro": intro}));
            let id = hub.current().unwrap().job_id;
            assert_eq!(job_height(&id), Some(h), "job_id {id} lost its height");
        }
        // Two same-height templates with different parents must be different jobs,
        // and both must still round-trip their height.
        hub.update(
            902,
            json!({"height": 902, "block_intro": intro_hex(902, 1_700_000_000, 0x11, 0xaa)}),
        );
        let a = hub.current().unwrap().job_id;
        hub.update(
            902,
            json!({"height": 902, "block_intro": intro_hex(902, 1_700_000_000, 0x22, 0xaa)}),
        );
        let b = hub.current().unwrap().job_id;
        assert_ne!(a, b, "a different parent must produce a different job id");
        assert_eq!(job_height(&a), Some(902));
        assert_eq!(job_height(&b), Some(902));
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
