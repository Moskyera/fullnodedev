//! Off-node reimplementation of the node's next-block difficulty rule, so the
//! pool can build templates the node accepts at REAL (mainnet) heights.
//!
//! This mirrors mint/src/check/difficulty_asert.rs exactly. Every detail below
//! is load-bearing — a value that is off by one means the node rejects the
//! block:
//!   * the exponent uses i128 `/` (truncates TOWARD ZERO, not floor)
//!   * num_shifts uses an arithmetic shift (floor) and the fraction is derived
//!     from it, so it is always in [0, 65535]
//!   * the polynomial adds (1<<47) BEFORE the >>48 truncation (round-half-up)
//!   * the target is shifted TWICE, separately (>> -num_shifts, then >> 16);
//!     fusing them changes the truncation
//!   * clamp order: zero-floor, then the 2x ease cap, then the LOWEST ceiling
//!
//! It returns BOTH representations, which are NOT interchangeable: the block
//! header must carry the u32 `num`, while the PoW comparison uses the exact
//! 32-byte target hash (which, on the from_big path, is more precise than
//! u32_to_hash(num)).

use basis::difficulty::*;
use num_bigint::BigUint;

const ASERT_START_TARGET_NUM: u32 = 0xe9cf_ffff;
const ASERT_HALF_LIFE: i128 = 10800;
const ASERT_RADIX: i128 = 1 << 16;
const ASERT_POLY_1: u128 = 195_766_423_245_049;
const ASERT_POLY_2: u128 = 971_821_376;
const ASERT_POLY_3: u128 = 5_127;
const ASERT_POLY_TERM_SHIFT: u32 = 48;
const ASERT_EASING_MAX_SCALE: u32 = 2;

/// The chain parameters the difficulty rule depends on.
#[derive(Clone, Debug)]
pub struct ChainParams {
    /// Height at which ASERT activates and which is also its anchor.
    pub asert_height: u64,
    /// `[mint] each_block_target_time` (mainnet 300s, testnet 10s).
    pub target_time: u64,
    /// Heights <= this use the bootstrap LOWEST_DIFFICULTY (testnet only).
    pub bootstrap_max: u64,
}

impl ChainParams {
    pub fn mainnet() -> Self {
        Self {
            asert_height: 738654,
            target_time: 300,
            bootstrap_max: 0,
        }
    }
    /// Non-mainnet: ASERT anchors at window+2 and heights <= window+1 bootstrap.
    pub fn testnet(adjust_blocks: u64, target_time: u64) -> Self {
        Self {
            asert_height: adjust_blocks + 2,
            target_time,
            bootstrap_max: adjust_blocks + 1,
        }
    }
    pub fn from_name(name: &str) -> Self {
        match name {
            "mainnet" => Self::mainnet(),
            _ => Self::testnet(288, 10),
        }
    }
    /// Does computing this height's difficulty need the anchor block's timestamp?
    pub fn needs_anchor(&self, height: u64) -> bool {
        height > self.asert_height
    }
}

/// Next block's difficulty as (header `difficulty` u32, PoW target hash).
pub fn next_difficulty(
    p: &ChainParams,
    height: u64,
    timestamp: u64,
    prev_difficulty: u32,
    anchor_time: u64,
) -> (u32, [u8; 32]) {
    if height <= p.bootstrap_max {
        let t = DifficultyTarget::from_num(LOWEST_DIFFICULTY);
        return (t.num, t.hash);
    }
    if height == p.asert_height {
        // Activation block: fixed start target, no parent cap.
        let t = DifficultyTarget::from_num(ASERT_START_TARGET_NUM);
        return (t.num, t.hash);
    }
    assert!(
        height > p.asert_height,
        "height {height} is in the pre-ASERT (legacy/LWMA) range, which this \
         off-node builder does not implement — a pool only mines at the tip"
    );

    let time_delta = timestamp as i128 - anchor_time as i128;
    let height_delta = height as i128 - p.asert_height as i128;
    // i128 division truncates toward zero. Multiply by the radix FIRST.
    let exponent =
        ((time_delta - p.target_time as i128 * height_delta) * ASERT_RADIX) / ASERT_HALF_LIFE;
    let num_shifts = exponent >> 16; // arithmetic shift == floor
    let frac = (exponent - (num_shifts << 16)) as u128; // always 0..=65535
    let frac2 = frac * frac;
    let frac3 = frac2 * frac;
    let factor = (((ASERT_POLY_1 * frac
        + ASERT_POLY_2 * frac2
        + ASERT_POLY_3 * frac3
        + (1u128 << (ASERT_POLY_TERM_SHIFT - 1)))
        >> ASERT_POLY_TERM_SHIFT)
        + 65536) as u64;

    let anchor_target = u32_to_biguint(ASERT_START_TARGET_NUM);
    let ease_target = u32_to_biguint(prev_difficulty) * BigUint::from(ASERT_EASING_MAX_SCALE);
    let max_target = u32_to_biguint(LOWEST_DIFFICULTY);

    let mut next = anchor_target * BigUint::from(factor);
    if num_shifts < 0 {
        next >>= (-num_shifts) as usize;
    } else if num_shifts > 0 {
        next <<= num_shifts as usize;
    }
    next >>= 16usize;

    if next == BigUint::from(0u8) {
        let t = DifficultyTarget::from_big(BigUint::from(1u8));
        return (t.num, t.hash);
    }
    if next > ease_target {
        next = ease_target; // never more than 2x easier than the parent
    }
    if next > max_target {
        let t = DifficultyTarget::from_num(LOWEST_DIFFICULTY);
        return (t.num, t.hash);
    }
    let t = DifficultyTarget::from_big(next);
    (t.num, t.hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_heights_use_lowest_difficulty() {
        let p = ChainParams::testnet(288, 10);
        let (num, hash) = next_difficulty(&p, 1, 1_000, 0, 0);
        assert_eq!(num, LOWEST_DIFFICULTY);
        assert_eq!(hash, DifficultyTarget::from_num(LOWEST_DIFFICULTY).hash);
        // the last bootstrap height is window+1
        assert_eq!(next_difficulty(&p, 289, 1_000, 0, 0).0, LOWEST_DIFFICULTY);
    }

    #[test]
    fn activation_height_uses_the_fixed_start_target() {
        let p = ChainParams::testnet(288, 10);
        let (num, hash) = next_difficulty(&p, 290, 9_999, LOWEST_DIFFICULTY, 0);
        assert_eq!(num, ASERT_START_TARGET_NUM);
        assert_eq!(hash, DifficultyTarget::from_num(ASERT_START_TARGET_NUM).hash);
        // mainnet anchors at 738654
        let m = ChainParams::mainnet();
        assert_eq!(
            next_difficulty(&m, 738654, 9_999, 0, 0).0,
            ASERT_START_TARGET_NUM
        );
    }

    #[test]
    fn on_schedule_reproduces_the_anchor_target() {
        // Exactly on schedule => exponent 0 => factor 65536 => target == anchor.
        let p = ChainParams::testnet(288, 10);
        let anchor_time = 1_000_000u64;
        let height = p.asert_height + 5;
        let timestamp = anchor_time + 5 * p.target_time; // perfectly on schedule
        let (num, _) = next_difficulty(&p, height, timestamp, ASERT_START_TARGET_NUM, anchor_time);
        assert_eq!(num, ASERT_START_TARGET_NUM);
    }

    #[test]
    fn faster_blocks_make_it_harder_slower_makes_it_easier() {
        let p = ChainParams::testnet(288, 10);
        let anchor_time = 1_000_000u64;
        let height = p.asert_height + 100;
        let on_time = anchor_time + 100 * p.target_time;
        let base = DifficultyTarget::from_num(
            next_difficulty(&p, height, on_time, ASERT_START_TARGET_NUM, anchor_time).0,
        );
        // ahead of schedule (mined too fast) -> smaller target (harder)
        let fast = DifficultyTarget::from_num(
            next_difficulty(&p, height, on_time - 600, ASERT_START_TARGET_NUM, anchor_time).0,
        );
        // behind schedule -> larger target (easier), capped at 2x the parent
        let slow = DifficultyTarget::from_num(
            next_difficulty(&p, height, on_time + 600, ASERT_START_TARGET_NUM, anchor_time).0,
        );
        assert!(fast.big < base.big, "faster blocks must tighten the target");
        assert!(slow.big > base.big, "slower blocks must ease the target");
    }

    #[test]
    fn easing_is_capped_at_twice_the_parent_target() {
        let p = ChainParams::testnet(288, 10);
        let anchor_time = 1_000_000u64;
        let height = p.asert_height + 10;
        // absurdly far behind schedule -> would explode, must clamp to 2x parent
        let prev = ASERT_START_TARGET_NUM;
        let (_, hash) = next_difficulty(&p, height, anchor_time + 10_000_000, prev, anchor_time);
        let cap = u32_to_biguint(prev) * BigUint::from(2u32);
        let got = DifficultyTarget::from_num(hash_to_u32(&hash)).big;
        assert!(got <= cap, "must never ease past 2x the parent target");
    }
}
