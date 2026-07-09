
macro_rules! delay_continue_ms {
    ($ms: expr) => {
        std::thread::sleep(std::time::Duration::from_millis($ms));
        continue
    }
}

#[allow(unused_macros)]
macro_rules! delay_continue {
    ($sec: expr) => {
        std::thread::sleep(std::time::Duration::from_secs($sec));
        continue 
    }
}

macro_rules! delay_return_ms {
    ($ms: expr) => {
        std::thread::sleep(std::time::Duration::from_millis($ms));
        return
    }
}

macro_rules! delay_return {
    ($sec: expr) => {
        std::thread::sleep(std::time::Duration::from_secs($sec));
        return
    }
}


#[allow(dead_code)]
fn hash_more_power(dst: &[u8], src: &[u8]) -> bool {
    let mut ln = dst.len();
    let l2 = src.len();
    if l2 < ln {
        ln = l2;
    }
    for i in 0..ln {
        let (l, r) = (dst[i], src[i]);
        if l < r {
            return true
        }else if l > r {
            return false
        }
    }
    return false
}

//
fn hash_left_zero_pad(dst: &[u8], pad: usize) -> Vec<u8> {
    let mut idx = 0usize;
    for i in 0 .. dst.len() {
        if dst[i] > 0 {
            idx = i;
            break
        }
    }
    dst[0 .. idx + pad].to_vec()
}


#[allow(dead_code)]
fn hash_left_zero_pad3(dst: &[u8]) -> Vec<u8> {
    hash_left_zero_pad(dst, 3)
}


#[allow(dead_code)]
fn diamond_more_power(dst: &[u8], src: &[u8]) -> bool {
    let o = b'0';
    for i in 0..16 {
        let (l, r) = (dst[i], src[i]);
        if l==o && r!=o {
            return true
        } else if l!=o && r==o {
            return false
        }else if l!=o && r!=o {
            return false
        }
    }
    false
}

fn resolve_gpu_tuning(sec_gpu: &std::collections::HashMap<String, Option<String>>) -> (u32, u32) {
    let profile = ini_must(sec_gpu, "gpu_profile", "");
    let mut workgroups = ini_must_u64(sec_gpu, "work_groups", 1024) as u32;
    let mut unitsize = ini_must_u64(sec_gpu, "unit_size", 128) as u32;
    match profile.as_str() {
        "amd_balanced" => {
            workgroups = 1024;
            unitsize = 128;
        }
        "amd_performance" => {
            workgroups = 2048;
            unitsize = 96;
        }
        "amd_max" => {
            workgroups = 4096;
            unitsize = 128;
        }
        _ => {}
    }
    (workgroups, unitsize)
}

