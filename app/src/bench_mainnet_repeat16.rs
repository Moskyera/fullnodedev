// bench_mainnet_repeat16.rs
//
// ADDITIVE MODULE — does not replace or modify any existing file.
// Drop-in benchmark that measures GPU block-mining throughput at the SAME
// X16RS round count the live Hacash mainnet uses (repeat = 16), and prints
// every raw number a reviewer needs to reproduce/verify the figure:
//
//   * x16rs repeat actually executed by the kernel (from block height)
//   * completed nonce count
//   * nonce range [start, end)
//   * summed kernel time (device) and wall-clock time
//   * CPU-verified batch count (every measured batch is re-hashed on the CPU
//     with x16rs::block_hash and byte-compared to the GPU result)
//   * raw hashrate (nonces/s) at repeat=16
//   * a back-to-back repeat=1 measurement + the repeat16:repeat1 ratio,
//     so the two numbers can be compared directly instead of guessed at.
//
// Why this exists:
//   The stock auto-tune benchmark (poworker::bench_block_hps) intentionally
//   measures at height = 1, i.e. x16rs repeat = 1, to keep tuning fast. That
//   is a legitimate *relative* signal for picking work_groups/unit_size, but
//   the resulting MH/s is NOT comparable to mainnet, where every nonce runs
//   the x16rs chain 16 times (see x16rs/opencl/x16rs.cl: `for r < x16rs_repeat`
//   and x16rs/src/block.rs: `repeat = min(16, height/50000 + 1)`).
//   This module produces the apples-to-apples, mainnet-representative number.
//
// See INTEGRATION.md for the two-line, fully reversible wiring.

#[cfg(feature = "ocl")]
use std::time::{Duration, Instant};

/// A height whose block_hash_repeat is the mainnet maximum of 16.
/// 800_000 / 50_000 + 1 = 17 -> clamped to 16 by x16rs::block_hash_repeat.
#[cfg(any(feature = "ocl", test))]
pub const MAINNET_REPEAT16_HEIGHT: u64 = 800_000;

/// A height whose block_hash_repeat is 1 (matches the stock auto-tune).
#[cfg(any(feature = "ocl", test))]
pub const REPEAT1_HEIGHT: u64 = 1;

#[cfg(any(feature = "ocl", test))]
const WARMUP_BATCHES: u32 = 3;
#[cfg(any(feature = "ocl", test))]
const MIN_VALID_SAMPLES: u32 = 5;

/// Fully-instrumented result of one measurement run at a fixed height/repeat.
#[cfg(any(feature = "ocl", test))]
#[derive(Clone, Debug)]
pub struct RepeatBenchReport {
    pub height: u64,
    pub repeat: u32,
    pub batch: u32,
    pub nonce_start: u32,
    pub nonce_end: u32,
    pub total_nonces: u64,
    pub measured_batches: u32,
    pub cpu_verified_batches: u32,
    pub kernel_seconds: f64,
    pub wall_seconds: f64,
    /// nonces per second, computed from summed kernel time.
    pub nonces_per_sec: f64,
}

#[cfg(any(feature = "ocl", test))]
impl RepeatBenchReport {
    /// Human-readable, copy-pasteable proof block for a community post.
    pub fn render(&self) -> String {
        format!(
            "  height           : {height}\n  \
             x16rs repeat     : {repeat}  (kernel inner loop ran {repeat}x per nonce)\n  \
             launch batch     : {batch} nonces/dispatch\n  \
             nonce range      : [{start}, {end})  ({total} nonces completed)\n  \
             measured batches : {mb}\n  \
             CPU-verified     : {cv}/{mb} batches (GPU best-nonce re-hashed with x16rs::block_hash, byte-equal)\n  \
             kernel time      : {kt:.3}s (device, summed)\n  \
             wall time        : {wt:.3}s\n  \
             hashrate         : {rate}  @ repeat={repeat}",
            height = self.height,
            repeat = self.repeat,
            batch = self.batch,
            start = self.nonce_start,
            end = self.nonce_end,
            total = self.total_nonces,
            mb = self.measured_batches,
            cv = self.cpu_verified_batches,
            kt = self.kernel_seconds,
            wt = self.wall_seconds,
            rate = fmt_rate(self.nonces_per_sec),
        )
    }
}

/// Format a nonces/sec figure as H/s, kH/s or MH/s (self-contained; does not
/// depend on basis::difficulty so this module stays drop-in).
#[cfg(any(feature = "ocl", test))]
pub fn fmt_rate(hps: f64) -> String {
    if hps >= 1_000_000.0 {
        format!("{:.2} MH/s", hps / 1_000_000.0)
    } else if hps >= 1_000.0 {
        format!("{:.2} kH/s", hps / 1_000.0)
    } else {
        format!("{:.2} H/s", hps)
    }
}

/// Re-hash the GPU's returned best nonce on the CPU and byte-compare.
/// Returns Ok(()) only when the GPU output is provably correct for `height`.
#[cfg(any(feature = "ocl", test))]
pub fn cpu_verify_batch(
    height: u64,
    block_intro: &[u8],
    nonce_start: u32,
    batch: u32,
    result_nonce: u32,
    result_hash: &[u8; 32],
) -> Result<(), String> {
    if batch == 0 {
        return Err("zero-size batch".to_string());
    }
    if result_hash.iter().all(|b| *b == 0) {
        return Err("GPU returned a zero hash".to_string());
    }
    if *result_hash == [u8::MAX; 32] {
        return Err("GPU returned no best hash".to_string());
    }
    if result_nonce.wrapping_sub(nonce_start) >= batch {
        return Err(format!(
            "GPU nonce {result_nonce} is outside batch starting at {nonce_start}"
        ));
    }
    if block_intro.len() < 83 {
        return Err("block intro too short".to_string());
    }
    let mut verify_intro = block_intro.to_vec();
    verify_intro[79..83].copy_from_slice(&result_nonce.to_be_bytes());
    let expected = x16rs::block_hash(height, &verify_intro);
    if expected != *result_hash {
        return Err(format!(
            "CPU verification FAILED: nonce={result_nonce} gpu={} cpu={}",
            hex::encode(result_hash),
            hex::encode(expected)
        ));
    }
    Ok(())
}

/// Run one instrumented measurement at a fixed height (=> fixed x16rs repeat).
#[cfg(feature = "ocl")]
pub fn measure_at_height(
    opencl: &crate::opencl_gpu::OpenCLResources,
    height: u64,
    workgroups: u32,
    localsize: u32,
    unitsize: u32,
    seconds: u64,
) -> Result<RepeatBenchReport, String> {
    use field::Serialize;
    use protocol::block::BlockIntro;

    let repeat = x16rs::block_hash_repeat(height) as u32;
    let block_intro = BlockIntro::default().serialize();

    let batch_u64 = (workgroups as u64)
        .saturating_mul(localsize as u64)
        .saturating_mul(unitsize as u64);
    if batch_u64 == 0 || batch_u64 > u32::MAX as u64 {
        return Err(format!(
            "invalid launch size wg={workgroups} local={localsize} unit_size={unitsize}"
        ));
    }
    let batch = batch_u64 as u32;

    let run_batch = |nonce_start: u32| -> Result<f64, String> {
        let started = Instant::now();
        let (result_nonce, result_hash) = crate::opencl_gpu::block::do_group_block_mining_opencl(
            opencl,
            height,
            block_intro.clone(),
            nonce_start,
            workgroups,
            localsize,
            unitsize,
        )
        .map_err(|e| e.display())?;
        let used = started.elapsed().as_secs_f64();
        if !used.is_finite() || used <= 0.0 {
            return Err("invalid kernel duration".to_string());
        }
        cpu_verify_batch(
            height,
            &block_intro,
            nonce_start,
            batch,
            result_nonce,
            &result_hash,
        )?;
        Ok(used)
    };

    // Warm-up (JIT / clocks / caches) — not counted.
    let mut nonce = 0u32;
    for w in 0..WARMUP_BATCHES {
        run_batch(nonce).map_err(|e| format!("warm-up batch {} failed: {e}", w + 1))?;
        nonce = nonce.wrapping_add(batch);
    }

    let nonce_start = nonce;
    let wall_start = Instant::now();
    let deadline = wall_start + Duration::from_secs(seconds.max(3));
    let mut kernel_seconds = 0.0f64;
    let mut measured_batches = 0u32;
    let mut cpu_verified_batches = 0u32;
    let mut total_nonces = 0u64;
    while Instant::now() < deadline {
        let used = run_batch(nonce)
            .map_err(|e| format!("measured batch {} failed: {e}", measured_batches + 1))?;
        // run_batch only returns Ok after a successful CPU verification.
        measured_batches += 1;
        cpu_verified_batches += 1;
        kernel_seconds += used;
        total_nonces = total_nonces.saturating_add(batch as u64);
        nonce = nonce.wrapping_add(batch);
    }
    let wall_seconds = wall_start.elapsed().as_secs_f64();

    if measured_batches < MIN_VALID_SAMPLES {
        return Err(format!(
            "only {measured_batches} valid samples (minimum {MIN_VALID_SAMPLES})"
        ));
    }
    if kernel_seconds <= 0.0 {
        return Err("zero measured kernel time".to_string());
    }
    let nonces_per_sec = total_nonces as f64 / kernel_seconds;

    Ok(RepeatBenchReport {
        height,
        repeat,
        batch,
        nonce_start,
        nonce_end: nonce,
        total_nonces,
        measured_batches,
        cpu_verified_batches,
        kernel_seconds,
        wall_seconds,
        nonces_per_sec,
    })
}

/// Initialize OpenCL from raw config values and run BOTH a mainnet repeat=16
/// measurement and a repeat=1 measurement back-to-back on the same device and
/// the same tuned launch config, then print a full, reproducible report.
#[cfg(feature = "ocl")]
#[allow(clippy::too_many_arguments)]
pub fn run_repeat_comparison(
    opencldir: &String,
    platformid: &u32,
    deviceids: &String,
    workgroups: &u32,
    localsize: &u32,
    unitsize: &u32,
    seconds: u64,
) {
    println!("[repeat16] Mainnet-representative benchmark (x16rs repeat=16).");
    println!(
        "[repeat16] Every measured batch is CPU-verified with x16rs::block_hash before it counts."
    );

    let scan = crate::opencl_diag::scan_opencl();
    let init_unitsize = (*unitsize).max(1);
    let resources = crate::opencl_gpu::initialize_opencl(
        false,
        opencldir,
        platformid,
        deviceids,
        workgroups,
        localsize,
        &init_unitsize,
        Some(&scan),
        false,
    );
    if resources.is_empty() {
        println!("[repeat16] No OpenCL devices.");
        return;
    }
    if resources.len() != 1 {
        println!(
            "[repeat16] Detected {} devices. Run one device at a time (set [gpu] device_ids to a single id) so the number is unambiguous.",
            resources.len()
        );
        return;
    }
    let opencl = &resources[0];

    let seconds = seconds.max(15);
    let r16 = measure_at_height(
        opencl,
        MAINNET_REPEAT16_HEIGHT,
        *workgroups,
        *localsize,
        *unitsize,
        seconds,
    );
    let r1 = measure_at_height(
        opencl,
        REPEAT1_HEIGHT,
        *workgroups,
        *localsize,
        *unitsize,
        seconds,
    );

    println!("\n================ REPEAT-16 (MAINNET) ================");
    match &r16 {
        Ok(rep) => println!("{}", rep.render()),
        Err(e) => println!("  REJECTED: {e}"),
    }
    println!("\n================ REPEAT-1  (auto-tune reference) ====");
    match &r1 {
        Ok(rep) => println!("{}", rep.render()),
        Err(e) => println!("  REJECTED: {e}"),
    }

    if let (Ok(a), Ok(b)) = (&r16, &r1) {
        if a.nonces_per_sec > 0.0 {
            let ratio = b.nonces_per_sec / a.nonces_per_sec;
            println!("\n================ SUMMARY ============================");
            println!(
                "  repeat=1  : {}\n  repeat=16 : {}\n  ratio     : {:.2}x  (expected ~16x; the repeat=1 figure is NOT a mainnet rate)",
                fmt_rate(b.nonces_per_sec),
                fmt_rate(a.nonces_per_sec),
                ratio
            );
            println!(
                "  On the live 16-round mainnet, THIS rig produces {} of block hashes.",
                fmt_rate(a.nonces_per_sec)
            );
        }
    }
    println!("====================================================");
}

/// Optional zero-touch trigger: if HACASH_REPEAT16_BENCH_SECONDS is set to a
/// positive integer, run the comparison and return true (caller should exit).
/// This lets you wire the module with a single `if ... { return; }` line.
#[cfg(feature = "ocl")]
pub fn run_from_env(
    opencldir: &String,
    platformid: &u32,
    deviceids: &String,
    workgroups: &u32,
    localsize: &u32,
    unitsize: &u32,
) -> bool {
    let Ok(raw) = std::env::var("HACASH_REPEAT16_BENCH_SECONDS") else {
        return false;
    };
    let secs: u64 = raw.trim().parse().unwrap_or(0);
    if secs == 0 {
        return false;
    }
    run_repeat_comparison(
        opencldir, platformid, deviceids, workgroups, localsize, unitsize, secs,
    );
    true
}

#[cfg(not(feature = "ocl"))]
pub fn run_from_env(
    _opencldir: &String,
    _platformid: &u32,
    _deviceids: &String,
    _workgroups: &u32,
    _localsize: &u32,
    _unitsize: &u32,
) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_height_yields_repeat_16() {
        assert_eq!(x16rs::block_hash_repeat(MAINNET_REPEAT16_HEIGHT), 16);
    }

    #[test]
    fn reference_height_yields_repeat_1() {
        assert_eq!(x16rs::block_hash_repeat(REPEAT1_HEIGHT), 1);
    }

    #[test]
    fn fmt_rate_scales_units() {
        assert_eq!(fmt_rate(250.0), "250.00 H/s");
        assert_eq!(fmt_rate(2_500.0), "2.50 kH/s");
        assert_eq!(fmt_rate(15_800_000.0), "15.80 MH/s");
    }

    #[test]
    fn cpu_verify_rejects_zero_and_out_of_range() {
        let intro = vec![0u8; 90];
        assert!(cpu_verify_batch(MAINNET_REPEAT16_HEIGHT, &intro, 0, 8, 0, &[0u8; 32]).is_err());
        assert!(cpu_verify_batch(MAINNET_REPEAT16_HEIGHT, &intro, 0, 8, 99, &[1u8; 32]).is_err());
    }
}
