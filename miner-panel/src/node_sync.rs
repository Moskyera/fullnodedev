//! Is the local full node actually caught up, or is it still downloading?
//!
//! The panel used to treat "the RPC answered" as ready and start mining. On a
//! node that is still fetching the chain that is not readiness, it is a promise
//! the miner cannot keep, and the way it fails is the worst kind: quietly, while
//! looking like success.
//!
//! Observed on a real machine. The node was at height 70,000 of a chain past
//! 800,000. The miner started anyway and reported 215 MH/s, a figure HIGHER than
//! a synced node produces, because `block_hash_repeat` is `height / 50000 + 1`
//! capped at 16: down at 70,000 each hash costs an eighth of what it costs at the
//! tip. So the operator saw a big number, believed it was working, and every
//! solution the card found was for a block the network had settled years before.
//!
//! Progress is measured in chain TIME, not in blocks, for a simple reason: the
//! height of the real tip is exactly the thing a node that has not synced does
//! not know. The timestamp of the block it DOES have, compared with the clock,
//! says how far back in history it is standing, and that needs nobody's help.

/// A mainnet block is aimed at 300 seconds, so a tip within half an hour is at
/// most a few blocks back. That is following the chain, not catching up to it.
pub const SYNCED_WITHIN_SECS: i64 = 1800;

/// What the node's chain looks like right now.
#[derive(Clone, Debug, PartialEq)]
pub struct SyncStatus {
    pub height: u64,
    /// Unix time of the block at `height`.
    pub tip_unix: i64,
    /// How far behind the wall clock that block is. Negative is clamped to 0:
    /// a tip slightly in the future is clock skew, not the future.
    pub behind_secs: i64,
    /// 0.0 to 1.0 of the chain's lifetime covered.
    pub progress: f32,
}

impl SyncStatus {
    pub fn is_synced(&self) -> bool {
        self.behind_secs <= SYNCED_WITHIN_SECS
    }

    /// Blocks still to fetch, give or take, at the mainnet target block time.
    pub fn blocks_behind(&self) -> u64 {
        (self.behind_secs / 300).max(0) as u64
    }
}

/// Turn the three timestamps into a status.
///
/// Split out from the fetching so it can be tested without a node: the edge
/// cases here (a tip ahead of the clock, a genesis at or after now, a chain of
/// zero length) are exactly the ones that would otherwise divide by zero or show
/// a bar running backwards in front of a user.
pub fn status_from(height: u64, tip_unix: i64, genesis_unix: i64, now_unix: i64) -> SyncStatus {
    let behind_secs = (now_unix - tip_unix).max(0);
    let span = now_unix - genesis_unix;
    let progress = if span <= 0 {
        // Nothing sensible to divide by. Report done rather than showing a bar
        // that cannot move; the behind_secs check still gates the mining start.
        1.0
    } else {
        (((tip_unix - genesis_unix) as f64) / (span as f64)).clamp(0.0, 1.0) as f32
    };
    SyncStatus {
        height,
        tip_unix,
        behind_secs,
        progress,
    }
}

/// Ask the node where it is. Blocking; call it from the poller thread.
///
/// Two requests because no single endpoint answers the question: `/query/latest`
/// gives the height it has reached, and `/query/block/intro` gives that block's
/// timestamp, which is the part that says whether the height means anything.
///
/// `None` means "could not tell", never "at genesis". Every caller treats it as
/// keep waiting, because a node that is mid-batch and briefly unresponsive must
/// not be mistaken for one that has finished.
pub fn probe(connect: &str) -> Option<SyncStatus> {
    let (_, latest) = crate::stats_poll::http_get(connect, "/query/latest").ok()?;
    let height = parse_height(&latest)?;
    let (_, intro) =
        crate::stats_poll::http_get(connect, &format!("/query/block/intro?height={height}"))
            .ok()?;
    let tip_unix = parse_timestamp(&intro)?;
    // Genesis is fetched once per probe rather than cached: it is one small
    // request against a local node, and caching it would mean carrying state
    // that goes stale if the operator points the panel at a different chain.
    let (_, genesis) = crate::stats_poll::http_get(connect, "/query/block/intro?height=1").ok()?;
    let genesis_unix = parse_timestamp(&genesis)?;
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(status_from(height, tip_unix, genesis_unix, now_unix))
}

/// The node's own refusal, verbatim from `mint/src/api/miner_pending.rs`:
///
/// ```text
/// if ctx.engine.config().is_mainnet() && !gotdmintx && curtimes() < ctx.launch_time + 30 {
///     return api_error("miner worker must be launched at least 30 secs after node start");
/// }
/// ```
///
/// A worker started inside that window gets one red `Error:` line in its log,
/// recovers by itself about fifteen seconds later, and teaches the operator that
/// red lines at startup are normal. That is how a real error becomes invisible.
const WORK_REFUSAL_MARKER: &str = "at least 30 secs after node start";

/// Whether the node will hand a worker something to mine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkReadiness {
    /// The node answered with something other than its startup refusal. That is
    /// deliberately not the same as "there is a template ready": any other
    /// answer, including a different error, means the refusal window is over,
    /// and everything past it is the worker's own retry loop to handle.
    Serving,
    /// Inside the node's 30 second post-start refusal window.
    StartupWindow,
    /// No usable answer. Never treated as ready, and never as an error either:
    /// a node mid-batch can be briefly unresponsive.
    Unknown,
}

/// Classify one `/query/miner/pending` body.
pub fn work_readiness_from(body: &str) -> WorkReadiness {
    if body.contains(WORK_REFUSAL_MARKER) {
        WorkReadiness::StartupWindow
    } else {
        WorkReadiness::Serving
    }
}

/// Ask the node the same question the worker is about to ask. Blocking; call it
/// from the poller thread.
///
/// This is the observable condition rather than a 30 second sleep on the panel
/// side: the window is counted from the node's own launch time, and the node may
/// have been started minutes ago by someone else, in which case there is nothing
/// to wait for at all.
pub fn probe_work(connect: &str) -> WorkReadiness {
    match crate::stats_poll::http_get(connect, "/query/miner/pending") {
        Ok((_, body)) => work_readiness_from(&body),
        Err(_) => WorkReadiness::Unknown,
    }
}

/// Pull `"height"` out of a `/query/latest` body.
pub fn parse_height(body: &str) -> Option<u64> {
    json_number(body, "height").map(|v| v as u64)
}

/// Pull `"timestamp"` out of a `/query/block/intro` body.
pub fn parse_timestamp(body: &str) -> Option<i64> {
    json_number(body, "timestamp")
}

/// Read one integer field out of a flat JSON object.
///
/// Deliberately not a JSON parser. These two endpoints return flat objects of
/// numbers and strings, the panel already avoids a JSON dependency in this
/// path, and a field that cannot be read must degrade to "unknown" rather than
/// to a wrong number: every caller here treats None as "keep waiting".
fn json_number(body: &str, key: &str) -> Option<i64> {
    let needle = format!("\"{key}\":");
    let start = body.find(&needle)? + needle.len();
    let rest = body.get(start..)?.trim_start();
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_part_way_through_the_chain_is_not_ready_to_mine_on() {
        // The real case: height 70,000, tip block from 2022, clock in 2026.
        let s = status_from(70_000, 1_666_000_000, 1_500_000_000, 1_785_181_634);
        assert!(!s.is_synced(), "a tip years old must not count as synced");
        assert!(
            s.progress > 0.5 && s.progress < 0.65,
            "progress {}",
            s.progress
        );
        assert!(s.blocks_behind() > 300_000);
    }

    #[test]
    fn a_tip_a_few_minutes_old_is_following_the_chain() {
        let now = 1_785_181_634;
        let s = status_from(812_345, now - 600, 1_500_000_000, now);
        assert!(
            s.is_synced(),
            "ten minutes behind is two blocks, not a resync"
        );
        assert_eq!(s.blocks_behind(), 2);
        assert!(s.progress > 0.999);
    }

    #[test]
    fn the_bar_never_runs_backwards_and_never_divides_by_zero() {
        let now = 1_785_181_634;
        // A tip in the future is clock skew, not a negative wait.
        let ahead = status_from(1, now + 5_000, 1_500_000_000, now);
        assert_eq!(ahead.behind_secs, 0);
        assert!(ahead.is_synced());
        assert!((0.0..=1.0).contains(&ahead.progress));
        // A genesis at or after the clock leaves nothing to divide by.
        let degenerate = status_from(1, now, now, now);
        assert_eq!(degenerate.progress, 1.0);
        let inverted = status_from(1, now, now + 10, now);
        assert!((0.0..=1.0).contains(&inverted.progress));
    }

    #[test]
    fn the_two_endpoint_bodies_this_reads_are_parsed() {
        // Captured verbatim from a running node.
        let latest = r#"{"diamond":4191,"height":70000,"ret":0}"#;
        assert_eq!(parse_height(latest), Some(70_000));

        let intro = r#"{"difficulty":3716737457,"hash":"0000000002738307","height":70000,"message":"bjpool         9","nonce":1761712708,"ret":0,"timestamp":1666268888}"#;
        assert_eq!(parse_timestamp(intro), Some(1_666_268_888));
        // "height" must not be mistaken for the timestamp, nor the reverse.
        assert_eq!(parse_height(intro), Some(70_000));
    }

    #[test]
    fn the_nodes_own_startup_refusal_is_recognised() {
        // The body a node inside its first 30 seconds returns.
        let refusal = r#"{"ret":1,"err":"miner worker must be launched at least 30 secs after node start"}"#;
        assert_eq!(work_readiness_from(refusal), WorkReadiness::StartupWindow);

        // A node serving work. Only the first fields matter here.
        let serving = r#"{"height":768567,"coinbase_nonce":"0000000000000000","block_intro":"01","target_hash":"000000000048deff","ret":0}"#;
        assert_eq!(work_readiness_from(serving), WorkReadiness::Serving);

        // Any OTHER error is not this gate's business: the worker retries, and
        // holding the start back on it would hang the panel on a node that is
        // never going to say anything different.
        let other = r#"{"ret":1,"err":"pending block not ready"}"#;
        assert_eq!(work_readiness_from(other), WorkReadiness::Serving);
        let disabled = r#"{"ret":1,"err":"miner not enabled"}"#;
        assert_eq!(work_readiness_from(disabled), WorkReadiness::Serving);
    }

    #[test]
    fn an_unreadable_body_is_unknown_rather_than_zero() {
        // Zero would read as "genesis", which is a plausible-looking lie. The
        // callers wait instead.
        assert_eq!(parse_height("not json at all"), None);
        assert_eq!(
            parse_timestamp(r#"{"ret":1,"err":"pending block not ready"}"#),
            None
        );
        assert_eq!(parse_height(r#"{"height":}"#), None);
    }
}
