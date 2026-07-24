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
}
