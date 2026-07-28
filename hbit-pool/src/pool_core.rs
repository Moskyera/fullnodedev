//! pool_core — the pool's off-node accounting brain. No consensus, no node
//! changes. Ties the two proven on-chain halves together: workers submit shares
//! (validated here) -> PPLNS accounting -> exact payout split -> the proven
//! batched settlement transfer.

use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use basis::difficulty::{DifficultyTarget, hash_bigger_than};

/// Wall-clock milliseconds since the epoch: the stamp every accepted share
/// carries, and the clock every credit figure is measured against.
///
/// MILLIseconds, not seconds, because credit is how long a share has been in the
/// window. A busy pool turns the whole 4096-share window over in well under a
/// second, and at one-second resolution every share would then read as zero-age:
/// the settlement would find nothing to split and no miner would ever be paid.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Pool share target, easier than the network target by 2^log2_factor. Workers
/// mine against this; on average ~1 in 2^log2_factor shares is also a real block.
/// An "easier" target is a LARGER 256-bit threshold, so we multiply the network
/// target by 2^log2_factor, saturating at the all-0xFF ceiling.
pub fn share_target_hash(network_difficulty: u32, log2_factor: u32) -> [u8; 32] {
    shift_left_saturating(network_target_hash(network_difficulty), log2_factor)
}

/// Multiply a big-endian 256-bit value by 2^bits, saturating to all-0xFF.
fn shift_left_saturating(mut h: [u8; 32], bits: u32) -> [u8; 32] {
    for _ in 0..bits {
        let mut carry: u8 = 0;
        for i in (0..32).rev() {
            let v = ((h[i] as u16) << 1) | carry as u16;
            h[i] = (v & 0xff) as u8;
            carry = (v >> 8) as u8;
        }
        if carry != 0 {
            return [0xff; 32]; // overflow -> easiest possible target
        }
    }
    h
}

/// The full network target for a difficulty (a share meeting THIS is a block).
pub fn network_target_hash(network_difficulty: u32) -> [u8; 32] {
    DifficultyTarget::from_num(network_difficulty).hash
}

/// Leading zero BITS of a big-endian 256-bit value.
fn leading_zero_bits(h: &[u8; 32]) -> u32 {
    let mut n = 0u32;
    for b in h {
        n += b.leading_zeros();
        if *b != 0 {
            break;
        }
    }
    n
}

/// How many powers of two easier than `network_target` the served `share_target`
/// ACTUALLY is, rounded down.
///
/// [`share_target_hash`] is asked for `network_target * 2^factor` but SATURATES
/// at the all-0xff ceiling, and says nothing when it does. On a low-difficulty
/// chain that is not a rounding detail: at the ceiling EVERY hash is a valid
/// share, so how many shares a worker is credited with stops depending on its
/// hashrate at all and becomes a measure of how fast it can submit. This is the
/// number that makes the difference visible - compare it with the configured
/// factor and the pool can refuse to serve work it could never account for
/// fairly.
pub fn achieved_share_factor(network_target: &[u8; 32], share_target: &[u8; 32]) -> u32 {
    leading_zero_bits(network_target).saturating_sub(leading_zero_bits(share_target))
}

/// What one share COSTS, as a power of two hashes, on the target actually served.
///
/// [`achieved_share_factor`] answers a different question - how much easier a
/// share is than a block - and the two can disagree in the direction that costs
/// money. On a chain whose network target has 22 leading zero bits, asking for a
/// factor of 24 saturates to the all-0xff ceiling: the achieved factor reads 22,
/// which clears any ratio bound comfortably, and yet EVERY hash beats the share
/// target. Credit then measures how fast a worker completes an HTTP round trip,
/// not how much it hashed, which is precisely what the ratio bound exists to
/// prevent. A ratio is only meaningful once the thing being divided costs
/// something, so this is the number that has to be checked first.
///
/// The two are related: `leading_zero_bits(network) == achieved + cost`. Lowering
/// the configured factor moves work from the first to the second, up to the
/// factor floor; below that the chain itself is too easy and nothing helps.
pub fn share_cost_bits(share_target: &[u8; 32]) -> u32 {
    leading_zero_bits(share_target)
}

/// The PoW hash of a serialized 89-byte block header.
pub fn hash_of(height: u64, header: &[u8]) -> [u8; 32] {
    x16rs::block_hash(height, header)
}

/// Does an already-computed hash meet `target`?
pub fn beats(hash: &[u8; 32], target: &[u8; 32]) -> bool {
    !hash_bigger_than(hash, target)
}

/// True if the solved 89-byte block header meets `target` (x16rs hash <= target).
pub fn meets_target(height: u64, header: &[u8], target: &[u8; 32]) -> bool {
    beats(&hash_of(height, header), target)
}

/// Buckets the banked-credit ledger is kept in. Credit leaves in whole buckets,
/// so this is the granularity at which a share's earned residence ages out.
const BANK_BUCKETS: u64 = 8;
/// Most workers one bucket of banked credit will hold. A worker only gets in
/// here by having a real share of its own evicted, so this is far above any
/// honest population; it exists so a flood of freshly-invented payout addresses
/// cannot grow this map without bound.
const BANK_WORKERS_MAX: usize = 65_536;

/// PPLNS accounting over a rolling window of the last `window` accepted shares.
///
/// Credit is NOT the headcount of that window. Every share is stamped with the
/// time it ARRIVED and earns credit for as long as it stays in the window,
/// capped at `horizon_ms`; what a share earned before it was evicted is banked
/// so a later arrival cannot erase it.
///
/// The headcount is what a withheld batch attacks. Nothing in a submission
/// carries a clock - the pool pins one template for a whole block interval and
/// rebuilds the header itself - so a share found at the start of an interval
/// verifies identically at the end of it. A miner could therefore sit on its
/// shares, dump the lot in the second before the settlement tick, and hold the
/// entire window at the instant the split is computed: every honest miner's
/// share is evicted by the burst and the withholder takes the whole payout. The
/// pool publishes both halves of that schedule itself in `/terms`.
///
/// Residence closes it. A share that has just arrived has earned nothing, so a
/// burst is worth nothing at the moment it lands, and the honest shares it
/// evicted keep the credit they had already earned. Submitting late is then
/// strictly worse than submitting promptly, which is the property the money
/// depends on.
#[derive(Debug)]
pub struct Pplns {
    window: usize,
    /// How long one share may go on earning credit, and how long credit banked
    /// by an evicted share survives. Bounded so a pool that goes quiet does not
    /// let one ancient share out-earn everyone still mining.
    horizon_ms: u64,
    /// (worker, arrival time in ms), oldest first.
    order: VecDeque<(String, u64)>,
    counts: HashMap<String, u64>,
    /// Credit (in milliseconds of residence) earned by shares that have already
    /// LEFT the window, bucketed by when they left so it ages out on the same
    /// horizon. Without it, evicting a share would destroy the credit it had
    /// already earned, and destroying other people's credit on demand is exactly
    /// what a burst of withheld shares does.
    banked: VecDeque<(u64, HashMap<String, u64>)>,
}

impl Pplns {
    pub fn new(window: usize, horizon_ms: u64) -> Self {
        Self {
            window: window.max(1),
            horizon_ms: horizon_ms.max(1),
            order: VecDeque::new(),
            counts: HashMap::new(),
            banked: VecDeque::new(),
        }
    }

    /// Record one accepted share from `worker`, stamped with the time it
    /// ARRIVED, evicting the oldest when full.
    ///
    /// `at_ms` is what makes withholding unprofitable, so it must be the pool's
    /// own clock reading at acceptance - never a value a worker supplied and
    /// never the time some later settlement happens to run.
    pub fn record(&mut self, worker: &str, at_ms: u64) {
        self.order.push_back((worker.to_string(), at_ms));
        // `counts.entry(k)` takes the key BY VALUE, so it would allocate a second
        // copy of the worker id on every share even when the worker is already
        // tracked. This runs under the pool's global lock on the hottest path, so
        // look the worker up by borrow and only allocate for a brand-new one.
        match self.counts.get_mut(worker) {
            Some(c) => *c += 1,
            None => {
                self.counts.insert(worker.to_string(), 1);
            }
        }
        while self.order.len() > self.window {
            if let Some((old, since)) = self.order.pop_front() {
                if let Some(c) = self.counts.get_mut(&old) {
                    *c -= 1;
                    if *c == 0 {
                        self.counts.remove(&old);
                    }
                }
                // Bank what that share earned while it was in the window. Drop it
                // instead and a miner able to push 4096 shares in at once would
                // wipe out every other miner's accrued credit for free.
                let earned = at_ms.saturating_sub(since).min(self.horizon_ms);
                self.bank(&old, earned, at_ms);
            }
        }
        self.expire_banked(at_ms);
    }

    /// Add `earned` milliseconds of residence to `worker`'s banked credit.
    fn bank(&mut self, worker: &str, earned: u64, at_ms: u64) {
        if earned == 0 {
            return;
        }
        let bucket = at_ms / self.bucket_ms();
        // A clock that steps backwards must never append an OLDER bucket after a
        // newer one: expiry pops from the front, so an out-of-order tail would
        // start dropping credit that is still current. Fold it into the newest
        // bucket instead.
        let fresh = match self.banked.back() {
            Some((b, _)) => bucket > *b,
            None => true,
        };
        if fresh {
            self.banked.push_back((bucket, HashMap::new()));
        }
        if let Some((_, rows)) = self.banked.back_mut() {
            match rows.get_mut(worker) {
                Some(v) => *v = v.saturating_add(earned),
                None => {
                    if rows.len() < BANK_WORKERS_MAX {
                        rows.insert(worker.to_string(), earned);
                    }
                }
            }
        }
    }

    fn bucket_ms(&self) -> u64 {
        (self.horizon_ms / BANK_BUCKETS).max(1)
    }

    /// Drop banked credit older than the horizon.
    fn expire_banked(&mut self, now_ms: u64) {
        let bucket = self.bucket_ms();
        while let Some((b, _)) = self.banked.front() {
            if now_ms.saturating_sub(b.saturating_mul(bucket)) > self.horizon_ms {
                self.banked.pop_front();
            } else {
                break;
            }
        }
    }

    /// What one share in the window has earned by `now_ms`.
    fn earned(&self, at_ms: u64, now_ms: u64) -> u64 {
        now_ms.saturating_sub(at_ms).min(self.horizon_ms)
    }

    /// worker -> credit (milliseconds of share residence) at `now_ms`, descending
    /// by credit then id. THIS is what a settlement splits money over.
    ///
    /// A settlement that split the raw headcount instead would be handing the
    /// whole payout to whoever submitted last, because the window is only 4096
    /// shares deep and one miner can fill all of it in a burst.
    pub fn credit(&self, now_ms: u64) -> Vec<(String, u64)> {
        let mut out: HashMap<&str, u64> = HashMap::new();
        for (bucket, rows) in &self.banked {
            if now_ms.saturating_sub(bucket.saturating_mul(self.bucket_ms())) > self.horizon_ms {
                continue;
            }
            for (w, ms) in rows {
                let e = out.entry(w.as_str()).or_insert(0);
                *e = e.saturating_add(*ms);
            }
        }
        for (w, at) in &self.order {
            let earned = self.earned(*at, now_ms);
            if earned == 0 {
                continue;
            }
            let e = out.entry(w.as_str()).or_insert(0);
            *e = e.saturating_add(earned);
        }
        let mut v: Vec<(String, u64)> = out
            .into_iter()
            .filter(|(_, c)| *c > 0)
            .map(|(w, c)| (w.to_string(), c))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// One worker's credit and the pool-wide total, in ONE pass.
    ///
    /// `/earnings` is polled by every miner and runs under the pool's global
    /// mutex, so it must never build (and sort) the whole credit table just to
    /// answer for one address. The two numbers are read together because a
    /// worker's pending estimate is the ratio of them, and reading them from two
    /// different instants would quote a fraction that was never true.
    pub fn credit_share(&self, worker: &str, now_ms: u64) -> (u64, u64) {
        let mut mine = 0u64;
        let mut total = 0u64;
        for (bucket, rows) in &self.banked {
            if now_ms.saturating_sub(bucket.saturating_mul(self.bucket_ms())) > self.horizon_ms {
                continue;
            }
            for (w, ms) in rows {
                total = total.saturating_add(*ms);
                if w == worker {
                    mine = mine.saturating_add(*ms);
                }
            }
        }
        for (w, at) in &self.order {
            let earned = self.earned(*at, now_ms);
            total = total.saturating_add(earned);
            if w == worker {
                mine = mine.saturating_add(earned);
            }
        }
        (mine, total)
    }

    /// The credit horizon in milliseconds, read out of the live struct so
    /// `/terms` states the one this process is running.
    pub fn horizon_ms(&self) -> u64 {
        self.horizon_ms
    }

    /// Number of shares currently in the window.
    pub fn total(&self) -> u64 {
        self.order.len() as u64
    }

    /// The window size N this accounting was built with: the last N accepted
    /// shares are the ones a payout is split over. Read out of the live struct so
    /// what the pool advertises is what the pool is running.
    pub fn window(&self) -> usize {
        self.window
    }

    /// Shares this one worker holds in the current window, in O(1).
    ///
    /// `/earnings` is polled by every miner, and it runs under the pool's global
    /// mutex like every other request: building the whole count table to answer
    /// for one worker would put a sort of up to N rows in front of every other
    /// miner's /work and /share.
    pub fn count_of(&self, worker: &str) -> u64 {
        self.counts.get(worker).copied().unwrap_or(0)
    }

    /// The raw window (oldest first) as (worker, arrival time), enough to
    /// persist and restore accounting so a pool restart never loses a miner's
    /// credited work.
    pub fn snapshot(&self) -> Vec<(String, u64)> {
        self.order.iter().cloned().collect()
    }

    /// The banked credit of shares already evicted, oldest first, each stamped
    /// with the WALL-CLOCK time in milliseconds its bucket starts at.
    ///
    /// A time, never the internal bucket index. The index is `time /
    /// bucket_ms`, and `bucket_ms` is derived from the settlement interval, so
    /// an index only means anything to a process running the same interval.
    /// Persisting it made a `settle_secs` change silently destroy or immortalise
    /// every miner's banked credit: shortening the interval multiplied the
    /// recovered timestamps down, so all of it read as ancient and expired at
    /// once, and lengthening it multiplied them up past `now`, so none of it
    /// ever expired again and every later eviction folded into that one
    /// immortal bucket. A time survives any interval.
    pub fn banked_snapshot(&self) -> Vec<(u64, Vec<(String, u64)>)> {
        let width = self.bucket_ms();
        self.banked
            .iter()
            .map(|(b, rows)| {
                let mut v: Vec<(String, u64)> =
                    rows.iter().map(|(w, ms)| (w.clone(), *ms)).collect();
                v.sort_by(|a, b| a.0.cmp(&b.0));
                (b.saturating_mul(width), v)
            })
            .collect()
    }

    /// Rebuild from a snapshot produced by [`Pplns::snapshot`] and
    /// [`Pplns::banked_snapshot`].
    ///
    /// The window is restored WITHOUT re-running eviction: replaying it through
    /// `record` would re-bank every share's residence a second time and hand
    /// long-standing miners double credit out of the pool's wallet.
    ///
    /// `banked` carries wall-clock TIMES, and the bucket each one belongs to is
    /// re-derived here from the horizon this process is running. Two stored
    /// times can land in the same bucket when the interval has been shortened;
    /// they are folded together rather than appended out of order, because
    /// expiry pops from the front.
    pub fn restore(
        window: usize,
        horizon_ms: u64,
        order: Vec<(String, u64)>,
        banked: Vec<(u64, Vec<(String, u64)>)>,
    ) -> Self {
        let mut p = Self::new(window, horizon_ms);
        let keep = p.window;
        // Newest first, pushed to the FRONT, so the surviving tail comes back in
        // the order it was accepted in.
        for (w, at) in order.into_iter().rev().take(keep) {
            p.order.push_front((w.clone(), at));
            match p.counts.get_mut(&w) {
                Some(c) => *c += 1,
                None => {
                    p.counts.insert(w, 1);
                }
            }
        }
        let width = p.bucket_ms();
        for (at_ms, rows) in banked {
            let bucket = at_ms / width;
            let mut map: HashMap<String, u64> = HashMap::new();
            for (w, ms) in rows.into_iter().take(BANK_WORKERS_MAX) {
                let e = map.entry(w).or_insert(0);
                *e = e.saturating_add(ms);
            }
            // Keep the persisted order: expiry pops from the front, so a bucket
            // out of sequence would drop credit that is still current.
            match p.banked.back() {
                Some((b, _)) if bucket <= *b => {
                    if let Some((_, back)) = p.banked.back_mut() {
                        for (w, ms) in map {
                            let e = back.entry(w).or_insert(0);
                            *e = e.saturating_add(ms);
                        }
                    }
                }
                _ => p.banked.push_back((bucket, map)),
            }
        }
        p
    }

    /// worker -> share count in the current window, descending by count then id.
    pub fn counts(&self) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = self.counts.iter().map(|(k, &c)| (k.clone(), c)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }
}

/// Split `reward_units` (smallest integer units) among workers by share count
/// using the largest-remainder method (exact — no unit created or lost), after
/// taking `fee_units` off the top. Workers whose payout is below `dust_units`
/// are dropped (their remainder stays with the pool). Returns (worker, units).
pub fn split_payout(
    reward_units: u64,
    fee_units: u64,
    dust_units: u64,
    counts: &[(String, u64)],
) -> Vec<(String, u64)> {
    let distributable = reward_units.saturating_sub(fee_units);
    let total_shares: u64 = counts.iter().map(|(_, c)| *c).sum();
    if distributable == 0 || total_shares == 0 {
        return vec![];
    }
    // floor split + remainder, exact via largest-remainder
    let mut rows: Vec<(String, u64, u128)> = Vec::with_capacity(counts.len());
    let mut assigned: u64 = 0;
    for (w, c) in counts {
        let exact = distributable as u128 * *c as u128;
        let floor = (exact / total_shares as u128) as u64;
        let rem = exact % total_shares as u128;
        assigned += floor;
        rows.push((w.clone(), floor, rem));
    }
    let mut leftover = distributable - assigned;
    rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    for row in rows.iter_mut() {
        if leftover == 0 {
            break;
        }
        row.1 += 1;
        leftover -= 1;
    }
    rows.into_iter()
        .filter(|(_, units, _)| *units >= dust_units && *units > 0)
        .map(|(w, units, _)| (w, units))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The horizon the pool really runs, so a test about a settlement-interval
    /// change uses the interval->horizon map the server uses.
    use crate::pplns_horizon_ms;

    #[test]
    fn shift_left_saturating_multiplies_and_saturates() {
        // 0x01 at byte 16, x16 (<<4) -> 0x10 at byte 16.
        let mut v = [0u8; 32];
        v[16] = 0x01;
        let mut want = [0u8; 32];
        want[16] = 0x10;
        assert_eq!(shift_left_saturating(v, 4), want);
        // overflow saturates to the easiest possible target.
        assert_eq!(shift_left_saturating([0xff; 32], 1), [0xff; 32]);
        assert_eq!(shift_left_saturating(v, 0), v);
    }

    #[test]
    fn share_target_is_never_harder_than_network() {
        let diff = 0x1000_0000u32;
        let net = network_target_hash(diff);
        // factor 0 == the network target exactly; any factor is >= it (easier).
        assert_eq!(share_target_hash(diff, 0), net);
        assert!(!hash_bigger_than(&net, &share_target_hash(diff, 4)));
    }

    #[test]
    fn meets_target_bounds() {
        let hdr = b"any 89-ish header bytes for the hash";
        assert!(meets_target(1, hdr, &[0xffu8; 32])); // easiest possible target
        assert!(!meets_target(1, hdr, &[0x00u8; 32])); // impossible target
    }

    /// A credit horizon comfortably longer than anything the tests below span,
    /// so the horizon cap is never what a test is measuring.
    const H: u64 = 600_000;

    #[test]
    fn pplns_window_evicts_oldest() {
        let mut p = Pplns::new(4, H);
        for (i, w) in ["a", "a", "b", "c", "d", "a"].into_iter().enumerate() {
            p.record(w, 1_000 * (i as u64 + 1));
        }
        // last 4 shares kept: [b, c, d, a]
        assert_eq!(p.total(), 4);
        let counts = p.counts();
        assert_eq!(counts.iter().map(|(_, c)| *c).sum::<u64>(), 4);
        let a = counts
            .iter()
            .find(|(w, _)| w == "a")
            .map(|(_, c)| *c)
            .unwrap();
        assert_eq!(a, 1);
    }

    #[test]
    fn pplns_survives_a_snapshot_restore_round_trip() {
        let mut p = Pplns::new(4, H);
        // More shares than the window, so some are evicted and their credit is
        // banked: a restart that loses the banked half would quietly cut every
        // long-standing miner's payout.
        for (i, w) in ["a", "b", "a", "c", "a", "b"].into_iter().enumerate() {
            p.record(w, 1_000 * (i as u64 + 1));
        }
        let restored = Pplns::restore(4, H, p.snapshot(), p.banked_snapshot());
        assert_eq!(restored.total(), p.total());
        assert_eq!(restored.counts(), p.counts());
        assert_eq!(restored.snapshot(), p.snapshot());
        let now = 20_000;
        assert_eq!(restored.credit(now), p.credit(now));
        assert!(!p.credit(now).is_empty());
        // Arrival times come back with the shares, so a restart does not reset
        // every share's age to zero and hand the next settlement to whoever
        // submits first after it.
        assert_eq!(restored.credit_share("a", now), p.credit_share("a", now));
    }

    #[test]
    fn one_workers_shares_are_readable_without_building_the_whole_table() {
        let mut p = Pplns::new(8, H);
        for (i, w) in ["a", "b", "a"].into_iter().enumerate() {
            p.record(w, 1_000 * (i as u64 + 1));
        }
        assert_eq!(p.count_of("a"), 2);
        assert_eq!(p.count_of("b"), 1);
        // A worker with nothing in the window reads as 0 here; only the CALLER
        // can tell that apart from a worker the pool has never heard of, which is
        // why /earnings never answers from this number alone.
        assert_eq!(p.count_of("never-seen"), 0);
        assert_eq!(p.window(), 8);
        // count_of always agrees with the full table it is a shortcut for.
        for (w, c) in p.counts() {
            assert_eq!(p.count_of(&w), c);
        }
    }

    #[test]
    fn a_withheld_batch_dumped_at_the_settlement_tick_earns_nothing() {
        // The attack this accounting exists to stop. Nothing a worker submits
        // carries a clock: the template is pinned for a whole block interval and
        // the pool rebuilds the header itself, so a share found at the start of
        // an interval verifies identically at the end of it. `/terms` publishes
        // both the window size and the settlement interval, so a miner can sit on
        // its shares and dump the lot in the second before the tick.
        //
        // "honest" submits steadily for the whole interval. "hoarder" mines the
        // same interval, submits nothing, then dumps a full window's worth at the
        // tick - which evicts every one of honest's shares.
        let window = 16;
        let mut p = Pplns::new(window, H);
        for i in 0..window as u64 {
            p.record("honest", 1_000 * (i + 1));
        }
        let tick = 100_000;
        for i in 0..window as u64 {
            p.record("hoarder", tick + i);
        }

        // By headcount the hoarder now owns the ENTIRE window: this is what a
        // settlement used to split real money over, and honest would have been
        // paid nothing at all.
        assert_eq!(p.count_of("hoarder"), window as u64);
        assert_eq!(p.count_of("honest"), 0);
        let by_headcount = split_payout(1_000, 0, 0, &p.counts());
        assert_eq!(by_headcount, vec![("hoarder".to_string(), 1_000)]);

        // By credit the dump is worth almost nothing - it has not been in the
        // window for any time - and honest keeps what its shares earned before
        // they were evicted.
        let credit = p.credit(tick + window as u64);
        let paid: HashMap<String, u64> = split_payout(1_000, 0, 0, &credit).into_iter().collect();
        let honest = *paid.get("honest").unwrap_or(&0);
        let hoarder = *paid.get("hoarder").unwrap_or(&0);
        assert!(
            honest > 900 && hoarder < 100,
            "withholding must not pay: honest={honest} hoarder={hoarder}"
        );
    }

    #[test]
    fn submitting_promptly_beats_withholding_for_the_same_work() {
        // The property the money rests on: for the SAME shares found at the same
        // times, holding them back can never pay more than sending them in. If it
        // ever does, every miner is better off withholding and the pool's split
        // stops measuring hashrate at all.
        let window = 64;
        let tick = 200_000;
        // A rival mining steadily throughout, so there is something to be diluted.
        let steady = |p: &mut Pplns| {
            for i in 0..48u64 {
                p.record("rival", 1_000 + i * 2_000);
            }
        };

        let mut prompt = Pplns::new(window, H);
        steady(&mut prompt);
        for i in 0..16u64 {
            prompt.record("miner", 2_000 + i * 6_000); // sent as found
        }
        let (prompt_mine, prompt_total) = prompt.credit_share("miner", tick);

        let mut held = Pplns::new(window, H);
        steady(&mut held);
        for i in 0..16u64 {
            held.record("miner", tick - 16 + i); // same shares, dumped at the tick
        }
        let (held_mine, held_total) = held.credit_share("miner", tick);

        // Compare the FRACTION of the pot, which is what a settlement pays.
        let prompt_cut = prompt_mine as u128 * held_total as u128;
        let held_cut = held_mine as u128 * prompt_total as u128;
        assert!(
            prompt_cut > held_cut,
            "withholding paid at least as much as submitting: prompt={prompt_mine}/{prompt_total} \
             held={held_mine}/{held_total}"
        );
    }

    #[test]
    fn steady_miners_are_still_split_by_hashrate() {
        // Credit must still be proportional for everyone mining normally, or the
        // fix for withholding would itself misallocate every payout: "big" finds
        // three shares for every one of "small", interleaved, over a stretch far
        // longer than the window.
        let mut p = Pplns::new(32, H);
        let mut t = 1_000u64;
        for _ in 0..200 {
            for _ in 0..3 {
                p.record("big", t);
                t += 250;
            }
            p.record("small", t);
            t += 250;
        }
        let credit: HashMap<String, u64> = p.credit(t).into_iter().collect();
        let big = credit["big"] as f64;
        let small = credit["small"] as f64;
        let ratio = big / small;
        assert!(
            (2.7..3.3).contains(&ratio),
            "three times the hashrate must earn about three times the credit, got {ratio}"
        );
    }

    #[test]
    fn credit_does_not_accrue_for_ever_after_a_pool_goes_quiet() {
        // Without the horizon, one share left in a window nobody else is mining
        // would go on earning for as long as the process runs, and the first
        // miner back would find a stranger holding the whole payout.
        let horizon = 10_000;
        let mut p = Pplns::new(8, horizon);
        p.record("old", 1_000);
        let (mine, _) = p.credit_share("old", 1_000 + horizon * 100);
        assert_eq!(mine, horizon);
        // Banked credit ages out on the same horizon rather than living for ever.
        let mut q = Pplns::new(1, horizon);
        q.record("gone", 1_000);
        q.record("here", 1_000 + horizon); // evicts "gone", banking its residence
        assert!(q.credit_share("gone", 1_000 + horizon).0 > 0);
        let much_later = 1_000 + horizon * 4;
        q.record("here", much_later);
        assert_eq!(q.credit_share("gone", much_later).0, 0);
    }

    #[test]
    fn banked_credit_survives_a_change_of_settlement_interval() {
        // `settle_secs` is an operator argument with documented bounds, and it
        // sizes the credit horizon, which sizes the banked-credit bucket. The
        // snapshot used to persist the bucket INDEX, so a restart under a
        // different interval reinterpreted it against a different bucket width
        // and moved real money in both directions:
        //
        //   shorter interval -> every recovered timestamp scaled DOWN, so all
        //     banked credit read as ancient and was expired on the spot. Every
        //     miner whose shares had rolled out of the window lost its whole cut
        //     of the next settlement.
        //   longer interval  -> every recovered timestamp scaled UP past `now`,
        //     so nothing ever expired again; that one bucket became immortal,
        //     every later eviction folded into it, and miners who stopped mining
        //     kept diluting the split for the life of the process.
        //
        // A wall clock means the same thing at every interval, so this is now
        // just a restart.
        let now = 1_700_000_000_000u64; // a real clock: the bug needed one
        let h300 = pplns_horizon_ms(300);
        let mut p = Pplns::new(1, h300);
        p.record("gone", now - 61_000);
        p.record("here", now - 1_000); // evicts "gone", banking 60s of residence
        let (order, banked) = (p.snapshot(), p.banked_snapshot());
        let earned = p.credit_share("gone", now).0;
        assert_eq!(earned, 60_000);

        // What goes on disk is a CLOCK READING, not an internal index. This is
        // the whole fix: an index is only meaningful to a process running the
        // same settlement interval, and it read as roughly now/37500 - a number
        // 37500 times too small - to any process running a different one.
        let (stamp, rows) = banked.first().cloned().expect("one banked bucket");
        assert_eq!(rows, vec![("gone".to_string(), 60_000)]);
        let bucket_ms = h300 / BANK_BUCKETS;
        assert!(
            stamp <= now - 1_000 && (now - 1_000) - stamp < bucket_ms,
            "the banked stamp must be the wall clock, got {stamp} against {now}"
        );

        // Same interval: unchanged, as before.
        let same = Pplns::restore(1, h300, order.clone(), banked.clone());
        assert_eq!(same.credit(now), p.credit(now));

        // SHORTER interval, with the credit still comfortably inside the new
        // horizon: it must survive. The index bug scaled the recovered time down
        // by the ratio of the two bucket widths, so credit banked a second ago
        // read as decades old and was expired on the spot.
        let h120 = pplns_horizon_ms(120);
        assert!(bucket_ms + 1_000 < h120, "the test must not sit on the edge");
        let shorter = Pplns::restore(1, h120, order.clone(), banked.clone());
        assert_eq!(
            shorter.credit_share("gone", now).0,
            earned,
            "shortening the settlement interval destroyed a miner's banked credit"
        );

        // LONGER interval: still expires on schedule instead of living for ever.
        // The index bug scaled the recovered time UP past `now`, so the age
        // saturated to zero, nothing was ever popped, and every later eviction
        // folded into that one immortal bucket.
        let h3600 = pplns_horizon_ms(3600);
        let mut longer = Pplns::restore(1, h3600, order, banked);
        assert_eq!(longer.credit_share("gone", now).0, earned);
        let much_later = now + h3600 * 4;
        longer.record("newcomer", much_later);
        assert_eq!(
            longer.credit_share("gone", much_later).0,
            0,
            "banked credit outlived its horizon after the interval changed"
        );
        // ...and the newcomer's own eviction opened a bucket of its own rather
        // than being swallowed by a bucket stamped in the far future.
        longer.record("newcomer", much_later + 1);
        assert!(
            longer
                .banked_snapshot()
                .iter()
                .all(|(at, _)| *at <= much_later + 1),
            "a bucket stamped after the clock can never expire"
        );
    }

    #[test]
    fn a_saturated_share_target_is_visible_as_a_lower_achieved_factor() {
        // Live-run defect: at a low network difficulty `share_target_hash` clamps
        // to the all-0xff ceiling, every hash becomes a valid share, and credit
        // stops tracking hashrate. Nothing reported it, because the pool only
        // ever looked at the CONFIGURED factor.
        // A difficulty whose target has a real mantissa AND room to shift: the
        // top byte encodes 255 - leading_zero_bits, so this is 223 leading zeros
        // followed by 24 significant bits.
        let diff = 0x20FF_FFFFu32;
        let net = network_target_hash(diff);
        assert_eq!(leading_zero_bits(&net), 223);
        for f in [0u32, 1, 4, 16] {
            assert_eq!(
                achieved_share_factor(&net, &share_target_hash(diff, f)),
                f,
                "with room to shift, the achieved factor IS the configured one"
            );
        }
        // The bootstrap difficulty: its target already has no leading zero bits,
        // so ANY factor saturates and the pool is really serving factor 0.
        let lowest = network_target_hash(basis::difficulty::LOWEST_DIFFICULTY);
        assert_eq!(leading_zero_bits(&lowest), 0);
        let served = share_target_hash(basis::difficulty::LOWEST_DIFFICULTY, 24);
        assert_eq!(served, [0xff; 32], "clamped to 'every hash is a share'");
        assert_eq!(
            achieved_share_factor(&lowest, &served),
            0,
            "24 was asked for and 0 is what the workers actually get"
        );
    }

    #[test]
    fn leading_zero_bits_counts_bits_not_bytes() {
        assert_eq!(leading_zero_bits(&[0xff; 32]), 0);
        assert_eq!(leading_zero_bits(&[0u8; 32]), 256);
        let mut v = [0u8; 32];
        v[0] = 0x01;
        assert_eq!(leading_zero_bits(&v), 7);
        let mut v = [0u8; 32];
        v[2] = 0x80;
        assert_eq!(leading_zero_bits(&v), 16);
    }

    #[test]
    fn split_is_proportional_and_exact() {
        let counts = vec![("a".to_string(), 3u64), ("b".to_string(), 1u64)];
        let out: HashMap<String, u64> = split_payout(100, 0, 0, &counts).into_iter().collect();
        assert_eq!(out["a"], 75);
        assert_eq!(out["b"], 25);
    }

    #[test]
    fn split_largest_remainder_loses_no_unit() {
        let counts = vec![
            ("a".to_string(), 1u64),
            ("b".to_string(), 1u64),
            ("c".to_string(), 1u64),
        ];
        let out = split_payout(100, 0, 0, &counts);
        assert_eq!(out.iter().map(|(_, u)| *u).sum::<u64>(), 100); // 34/33/33
    }

    #[test]
    fn split_takes_fee_and_drops_dust() {
        let counts = vec![("big".to_string(), 99u64), ("tiny".to_string(), 1u64)];
        let out: HashMap<String, u64> = split_payout(1000, 100, 20, &counts).into_iter().collect();
        // distributable 900: big=891, tiny=9 < dust 20 -> dropped
        assert_eq!(out.get("big"), Some(&891));
        assert!(!out.contains_key("tiny"));
    }
}
