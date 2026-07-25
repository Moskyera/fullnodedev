//! Shared hash / diamond comparison helpers for CPU and OpenCL mining paths.

pub fn hash_more_power(dst: &[u8], src: &[u8]) -> bool {
    let mut ln = dst.len();
    let l2 = src.len();
    if l2 < ln {
        ln = l2;
    }
    for i in 0..ln {
        let (l, r) = (dst[i], src[i]);
        if l < r {
            return true;
        } else if l > r {
            return false;
        }
    }
    false
}

pub fn hash_left_zero_pad3(dst: &[u8]) -> Vec<u8> {
    let mut idx = 0usize;
    for (i, &byte) in dst.iter().enumerate() {
        if byte > 0 {
            idx = i;
            break;
        }
    }
    // Clamp the end: a degenerate hash whose first non-zero byte sits in the last
    // two bytes (or an input shorter than 3 bytes) would otherwise slice past the
    // end and panic inside the sole result/submit thread.
    let end = (idx + 3).min(dst.len());
    dst[..end].to_vec()
}

pub fn diamond_more_power(dst: &[u8], src: &[u8]) -> bool {
    let o = b'0';
    for i in 0..dst.len().min(src.len()) {
        let (l, r) = (dst[i], src[i]);
        if l == o && r != o {
            return true;
        } else if l != o && r == o {
            return false;
        } else if l != o && r != o {
            return false;
        }
    }
    false
}

/// Mainnet consensus name shape: exactly DMD_L leading '0' chars followed by
/// DMD_M-DMD_L non-'0' chars. Mirrors `x16rs::check_diamond_hash_result` (and
/// `diamond_is_valid_name` in x16rs_diamond.cl) without allocating.
pub fn diamond_name_is_valid(dia: &[u8]) -> bool {
    const DMD_L: usize = 10;
    const DMD_M: usize = 16;
    if dia.len() != DMD_M {
        return false;
    }
    dia[..DMD_L].iter().all(|c| *c == b'0') && dia[DMD_L..].iter().all(|c| *c != b'0')
}

/// True when candidate `dst` should replace the current best `src`.
///
/// `diamond_more_power` on its own is pure more-leading-zeros-wins, so it ranks
/// an 11+ zero overshoot (which can never be minted) above a real diamond. The
/// GPU kernel already ranks valid-first; this is the same rule on the host so
/// kernel, host and the console "best so far" cannot disagree.
pub fn diamond_better(dst: &[u8], src: &[u8]) -> bool {
    match (diamond_name_is_valid(dst), diamond_name_is_valid(src)) {
        (true, false) => true,
        (false, true) => false,
        _ => diamond_more_power(dst, src),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_pad3_never_slices_past_the_end() {
        // An upstream-supplied target with 30+ leading zero bytes used to panic
        // here and kill the result/submit thread.
        let mut degenerate = [0u8; 32];
        degenerate[31] = 1;
        assert_eq!(hash_left_zero_pad3(&degenerate).len(), 32);
        let mut near_end = [0u8; 32];
        near_end[30] = 1;
        assert_eq!(hash_left_zero_pad3(&near_end).len(), 32);
        assert_eq!(hash_left_zero_pad3(&[0u8; 32]).len(), 3);
        assert!(hash_left_zero_pad3(&[]).is_empty());
        assert_eq!(hash_left_zero_pad3(&[0u8, 1u8]).len(), 2);
    }

    #[test]
    fn zero_pad3_keeps_three_bytes_after_the_first_non_zero() {
        let mut normal = [0u8; 32];
        normal[2] = 9;
        assert_eq!(hash_left_zero_pad3(&normal), vec![0u8, 0u8, 9u8, 0u8, 0u8]);
    }

    const MINTABLE: &[u8; 16] = b"0000000000WTYUIA";
    const OVERSHOOT: &[u8; 16] = b"00000000000TYUIA";
    const WEAK: &[u8; 16] = b"000000000WTYUIAH";

    #[test]
    fn a_valid_diamond_name_is_exactly_ten_zeros_then_six_non_zeros() {
        assert!(diamond_name_is_valid(MINTABLE));
        // 11 leading zeros: "more power" but the node rejects it.
        assert!(!diamond_name_is_valid(OVERSHOOT));
        // 9 leading zeros: below the bar.
        assert!(!diamond_name_is_valid(WEAK));
        assert!(!diamond_name_is_valid(b"0000000000WTYUI"));
        assert!(!diamond_name_is_valid(&[0u8; 16]));
    }

    #[test]
    fn the_ranking_prefers_a_mintable_diamond_over_a_stronger_overshoot() {
        // This is the defect: the raw leading-zero rule ranks the unmintable
        // 11-zero hash above the real diamond, so the GPU work group and the
        // console "best" both report a hash the miner would never submit.
        assert!(diamond_more_power(OVERSHOOT, MINTABLE));
        assert!(!diamond_better(OVERSHOOT, MINTABLE));
        assert!(diamond_better(MINTABLE, OVERSHOOT));
        // Same rule as the kernel: valid beats invalid, otherwise fall back to
        // leading zeros.
        assert!(diamond_better(MINTABLE, WEAK));
        assert!(!diamond_better(WEAK, MINTABLE));
        assert!(diamond_better(OVERSHOOT, WEAK));
        assert!(!diamond_better(MINTABLE, MINTABLE));
    }
}
