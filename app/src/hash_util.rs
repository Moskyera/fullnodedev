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
    dst[0..idx + 3].to_vec()
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
