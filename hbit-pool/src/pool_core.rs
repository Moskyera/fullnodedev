//! pool_core — the pool's off-node accounting brain. No consensus, no node
//! changes. Ties the two proven on-chain halves together: workers submit shares
//! (validated here) -> PPLNS accounting -> exact payout split -> the proven
//! batched settlement transfer.

use std::collections::{HashMap, VecDeque};

use basis::difficulty::{DifficultyTarget, hash_bigger_than};

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

/// PPLNS accounting over a rolling window of the last `window` accepted shares.
#[derive(Debug)]
pub struct Pplns {
    window: usize,
    order: VecDeque<String>,
    counts: HashMap<String, u64>,
}

impl Pplns {
    pub fn new(window: usize) -> Self {
        Self {
            window: window.max(1),
            order: VecDeque::new(),
            counts: HashMap::new(),
        }
    }

    /// Record one accepted share from `worker`, evicting the oldest when full.
    pub fn record(&mut self, worker: &str) {
        self.order.push_back(worker.to_string());
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
            if let Some(old) = self.order.pop_front() {
                if let Some(c) = self.counts.get_mut(&old) {
                    *c -= 1;
                    if *c == 0 {
                        self.counts.remove(&old);
                    }
                }
            }
        }
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

    /// The raw window (oldest first) — enough to persist and restore accounting
    /// so a pool restart never loses a miner's credited work.
    pub fn snapshot(&self) -> Vec<String> {
        self.order.iter().cloned().collect()
    }

    /// Rebuild from a snapshot produced by [`Pplns::snapshot`].
    pub fn restore(window: usize, order: Vec<String>) -> Self {
        let mut p = Self::new(window);
        for w in order {
            p.record(&w);
        }
        p
    }

    /// worker -> share count in the current window, descending by count then id.
    pub fn counts(&self) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> =
            self.counts.iter().map(|(k, &c)| (k.clone(), c)).collect();
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

    #[test]
    fn pplns_window_evicts_oldest() {
        let mut p = Pplns::new(4);
        for w in ["a", "a", "b", "c", "d", "a"] {
            p.record(w);
        }
        // last 4 shares kept: [b, c, d, a]
        assert_eq!(p.total(), 4);
        let counts = p.counts();
        assert_eq!(counts.iter().map(|(_, c)| *c).sum::<u64>(), 4);
        let a = counts.iter().find(|(w, _)| w == "a").map(|(_, c)| *c).unwrap();
        assert_eq!(a, 1);
    }

    #[test]
    fn pplns_survives_a_snapshot_restore_round_trip() {
        let mut p = Pplns::new(8);
        for w in ["a", "b", "a", "c", "a"] {
            p.record(w);
        }
        let restored = Pplns::restore(8, p.snapshot());
        assert_eq!(restored.total(), p.total());
        assert_eq!(restored.counts(), p.counts());
        assert_eq!(
            restored.counts().iter().find(|(w, _)| w == "a").unwrap().1,
            3
        );
    }

    #[test]
    fn one_workers_shares_are_readable_without_building_the_whole_table() {
        let mut p = Pplns::new(8);
        for w in ["a", "b", "a"] {
            p.record(w);
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
        let out: HashMap<String, u64> =
            split_payout(1000, 100, 20, &counts).into_iter().collect();
        // distributable 900: big=891, tiny=9 < dust 20 -> dropped
        assert_eq!(out.get("big"), Some(&891));
        assert!(!out.contains_key("tiny"));
    }
}
