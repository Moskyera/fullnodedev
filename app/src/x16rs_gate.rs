//! x16rs equivalence gate and fixed-corpus baseline.
//!
//! ADDITIVE MODULE. Nothing in the mining path calls it; it exists so that any
//! future change to the OpenCL kernel can be judged against two things that a
//! self-test cannot give you:
//!
//!   1. `equiv`, a byte-equivalence proof. A fixed corpus of block headers and
//!      a fixed nonce window, hashed on the GPU and on the CPU, with EVERY 32
//!      byte result compared exactly, at x16rs repeat = 1, 4, 8 and 16. The
//!      oracle is the CPU consensus implementation (`x16rs::block_hash`, i.e.
//!      the x16rs-sys C reference), never another GPU build, so a bug that both
//!      GPU builds share is still caught.
//!
//!   2. `baseline`, a FIXED-WORK timing run. x16rs per-hash cost varies by
//!      algorithm, so a fixed-TIME benchmark silently compares different work
//!      between runs. This one hashes an identical, deterministic nonce range
//!      every time and reports the spread across repeated runs, which is the
//!      noise floor any later "optimisation" has to beat to mean anything.
//!
//! How `equiv` gets every hash off the card. The mining kernel returns only the
//! best hash per work group, which would prove one hash in a hundred thousand.
//! The pool share list (`x16rs_main.cl`, `share_capacity != 0`) already emits
//! (nonce, hash) for every nonce that beats a target. With target 0xff..ff every
//! nonce qualifies, so a batch sized to exactly `SHARE_LIST_CAPACITY` nonces
//! dumps its ENTIRE window. That is the exhaustive mode. The production launch
//! shape (48 x 256 x 48 = 589 824 nonces) cannot fit in the list, so there the
//! gate checks the 1024 sampled hashes byte for byte AND checks that the kernel
//! counted the whole window.

#[cfg(feature = "ocl")]
use std::time::Instant;

/// Height whose `block_hash_repeat` is the mainnet maximum, 16.
pub const REPEAT16_HEIGHT: u64 = 800_000;

/// The 89-byte block intro layout the kernel and the CPU agree on.
pub const BLOCK_INTRO_BYTES: usize = 89;

/// Byte offset of the 4-byte big-endian nonce inside the intro.
pub const NONCE_OFFSET: usize = 79;

/// Heights that produce repeat = 1, 4, 8 and 16. `block_hash_repeat` is
/// `min(16, height / 50_000 + 1)`, so these are exact, not approximate.
pub const GATE_HEIGHTS: [u64; 4] = [1, 150_000, 350_000, 800_000];

/// Deterministic 89-byte block intro number `index`.
///
/// Every byte is pseudo-random so that the corpus is not a family of near
/// identical inputs, but the stream is a fixed SplitMix64 seeded by `index`, so
/// two runs a month apart hash exactly the same bytes.
///
/// Two constraints are not free choices:
///   * bytes 79..83 are the nonce and are overwritten per hash;
///   * byte 88 MUST be 0. `sha3_256.cl` folds the 89-byte message's padding into
///     a constant that pins byte 88 to 0x00 (see the comment at sha3_256.cl:154).
///     A real intro satisfies this because byte 88 is the low byte of
///     `witness_stage`. A corpus that ignored it would have the card and the CPU
///     hashing different messages, and the gate would report a kernel bug that
///     is really a harness bug.
pub fn corpus_header(index: u32) -> Vec<u8> {
    let mut state = 0x9E3779B97F4A7C15u64 ^ ((index as u64).wrapping_mul(0xD1B54A32D192ED03));
    let mut next = || -> u64 {
        state = state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    };
    let mut intro = vec![0u8; BLOCK_INTRO_BYTES];
    for chunk in intro.chunks_mut(8) {
        let word = next().to_le_bytes();
        let take = chunk.len();
        chunk.copy_from_slice(&word[..take]);
    }
    intro[BLOCK_INTRO_BYTES - 1] = 0;
    intro
}

/// The CPU oracle for one nonce. This is the consensus hash, byte for byte.
pub fn cpu_hash(height: u64, intro: &[u8], nonce: u32) -> [u8; 32] {
    let mut stuff = intro.to_vec();
    stuff[NONCE_OFFSET..NONCE_OFFSET + 4].copy_from_slice(&nonce.to_be_bytes());
    x16rs::block_hash(height, &stuff)
}

/// CPU oracle for a whole contiguous nonce window, spread over `threads`.
///
/// Threading changes nothing about the result: each nonce is independent and the
/// output is reassembled in nonce order. It changes a great deal about whether
/// the gate is run at all, so the cost is worth stating in numbers rather than
/// in adjectives.
///
/// Measured in a release build on this machine, `x16rs::block_hash` at height
/// 800 000 (repeat = 16) runs at 78 630 hashes a second on one core. A caller
/// asking for a whole production window is therefore asking for
/// `window / CPU_ORACLE_HPS_PER_CORE` core-seconds and `window * 32` bytes twice
/// over: a 3.1 M-nonce window (64x256x192, the largest an RX 9070 XT can be
/// asked for) is under four seconds on fourteen threads and 200 MB, while a
/// 100 M-nonce window (3072x256x128, which a 3584 work-group preset permits) is
/// two minutes and 6.4 GB. That is why
/// `autotune16::plan_session` prunes the shapes whose batch cannot meet the
/// latency ceiling before anything proves them: the proof cost is linear in the
/// launch window, and the largest windows are the ones that could never have
/// been chosen anyway.
///
/// The note that stood here said "a few hundred hashes a second per core". That
/// is not what this build does, and the gap matters: it is the difference
/// between a production-window proof being impossible and it being a hundred
/// seconds.
/// Hashes a second one core manages on `x16rs::block_hash` at repeat 16.
///
/// Measured on this machine over 20 000 nonces: 78 630 H/s in a release build,
/// 16 268 H/s in a debug one. 60 000 is quoted rather than 78 000 because the
/// number's only job is to tell an operator how long a proof will take, the
/// machine running the tune is also mining, and an estimate that under-states
/// the wait is worse than one that over-states it.
pub const CPU_ORACLE_HPS_PER_CORE: f64 = 60_000.0;

/// How long `cpu_hash_window` will take, and how much it will allocate, for a
/// window of `window` nonces on `threads` threads.
///
/// The caller is the auto-tuner, which has to be able to tell the operator what
/// a tune will cost before it starts spending their time on it.
pub fn oracle_cost(window: u64, threads: usize) -> (f64, u64) {
    let threads = threads.clamp(1, 256) as f64;
    (
        window as f64 / (CPU_ORACLE_HPS_PER_CORE * threads),
        // `cpu` and its sorted clone, 32 bytes a nonce each.
        window.saturating_mul(64),
    )
}

pub fn cpu_hash_window(
    height: u64,
    intro: &[u8],
    nonce_start: u32,
    count: u32,
    threads: usize,
) -> Vec<[u8; 32]> {
    let threads = threads.clamp(1, 256);
    if threads == 1 || count < threads as u32 {
        return (0..count)
            .map(|i| cpu_hash(height, intro, nonce_start.wrapping_add(i)))
            .collect();
    }
    let mut out: Vec<[u8; 32]> = vec![[0u8; 32]; count as usize];
    let chunk = count.div_ceil(threads as u32) as usize;
    std::thread::scope(|scope| {
        for (slot, piece) in out.chunks_mut(chunk).enumerate() {
            let base = nonce_start.wrapping_add((slot * chunk) as u32);
            scope.spawn(move || {
                for (i, cell) in piece.iter_mut().enumerate() {
                    *cell = cpu_hash(height, intro, base.wrapping_add(i as u32));
                }
            });
        }
    });
    out
}

/// The algorithm index x16rs picks for each round, computed on the CPU.
///
/// The reference picks `inputoutput[7] % 16` at the top of every round, where
/// `inputoutput` is the 32-byte state read as little-endian u32s. Word 7 is
/// bytes 28..32, so `% 16` is the low nibble of byte 28. Re-deriving it here
/// costs `repeat` extra CPU hashes per nonce and needs no change to the C
/// reference, which is the point: the oracle stays untouched.
pub fn algo_sequence(height: u64, intro: &[u8], nonce: u32) -> Vec<u8> {
    let repeat = x16rs::block_hash_repeat(height);
    let mut stuff = intro.to_vec();
    stuff[NONCE_OFFSET..NONCE_OFFSET + 4].copy_from_slice(&nonce.to_be_bytes());
    let seed = x16rs::calculate_hash(&stuff);
    let mut seq = Vec::with_capacity(repeat as usize);
    for round in 0..repeat {
        let state = x16rs_sys_hash(round, &seed);
        seq.push(state[28] & 0x0f);
    }
    seq
}

/// `x16rs_hash` with an explicit loop count, including 0 (identity).
fn x16rs_sys_hash(loops: i32, input: &[u8; 32]) -> [u8; 32] {
    x16rs::x16rs_hash(loops, input)
}

/// One mismatch, with everything needed to reproduce it by hand.
#[derive(Clone, Debug)]
pub struct Mismatch {
    pub height: u64,
    pub repeat: i32,
    pub header_index: u32,
    pub nonce: u32,
    pub gpu: [u8; 32],
    pub cpu: [u8; 32],
    /// Algorithm indices the CPU used, in round order.
    pub algos: Vec<u8>,
}

impl Mismatch {
    pub fn render(&self) -> String {
        format!(
            "  MISMATCH height={} repeat={} header={} nonce={}\n    gpu={}\n    cpu={}\n    cpu algo order={:?}",
            self.height,
            self.repeat,
            self.header_index,
            self.nonce,
            hex::encode(self.gpu),
            hex::encode(self.cpu),
            self.algos,
        )
    }
}

/// Totals for one full `equiv` run.
#[derive(Clone, Debug, Default)]
pub struct EquivReport {
    pub compared: u64,
    pub mismatches: Vec<Mismatch>,
    /// How many times each of the 16 algorithms was actually executed by the
    /// compared corpus, according to the CPU. A zero here means the gate never
    /// tested that algorithm, which is a hole in the gate, not a pass.
    pub algo_counts: [u64; 16],
    pub exhaustive_batches: u32,
    pub production_batches: u32,
    pub production_nonces: u64,
    /// Launches whose full-window hit count was compared against the CPU's.
    pub production_count_checks: u32,
    /// Production windows whose best-hash reduction was proved to be the true
    /// CPU minimum of the window.
    pub production_reduction_checks: u32,
    pub wall_seconds: f64,
}

/// Rank thresholds for the production-shape count check.
///
/// `levels` evenly spaced ranks, plus a few tiny ones so that some launches
/// return a share list short enough to be complete, plus `capacity` itself so
/// exactly one launch fills the list and every byte of it is compared.
pub fn threshold_ranks(window: u64, capacity: u64, levels: u32) -> Vec<u64> {
    let levels = levels.max(1) as u64;
    let mut ranks = vec![1u64, 2, 16, 256, capacity.min(window)];
    for step in 1..=levels {
        ranks.push((window.saturating_mul(step) / (levels + 1)).max(1));
    }
    ranks.retain(|rank| *rank >= 1 && *rank <= window);
    ranks.sort_unstable();
    ranks.dedup();
    ranks
}

/// Probability that ONE wrong hash anywhere in the window slips past EVERY
/// count threshold.
///
/// The thresholds cut the window's sorted hashes into bins. A count changes
/// only when the true hash and the corrupted one fall on opposite sides of some
/// threshold, so the check misses exactly when the corrupted value lands in the
/// same bin as the true one. Assuming a corrupted hash is uniform over the
/// 256-bit range and the true hash is at a uniformly random rank, that is the
/// sum of the squared bin widths.
///
/// The naive "one minus the product over thresholds" is WRONG here: the
/// threshold outcomes are not independent, they are all determined by where the
/// one corrupted value lands. Getting this right matters, because the wrong
/// formula makes 28 thresholds look like p = 1e-4 when the truth is p = 3e-2.
pub fn threshold_miss_probability(window: u64, ranks: &[u64]) -> f64 {
    if window == 0 {
        return 1.0;
    }
    let mut edges: Vec<f64> = ranks.iter().map(|r| *r as f64 / window as f64).collect();
    edges.push(1.0);
    let mut previous = 0.0f64;
    let mut sum = 0.0f64;
    for edge in edges {
        let width = (edge - previous).max(0.0);
        sum += width * width;
        previous = edge;
    }
    sum
}

impl EquivReport {
    pub fn passed(&self) -> bool {
        self.mismatches.is_empty()
            && self.compared > 0
            && self.algo_counts.iter().all(|count| *count > 0)
    }

    pub fn render(&self) -> String {
        let mut text = String::new();
        text.push_str(&format!(
            "  hashes compared byte-for-byte : {}\n  \
             exhaustive batches (ENTIRE window dumped and compared) : {}\n  \
             production-shape windows : {} ({} nonces, all CPU-hashed)\n  \
             production full-window count checks : {} (each reads all {} GPU hashes)\n  \
             production best-hash reductions proved minimal : {}\n  \
             mismatches : {}\n  wall : {:.1}s\n",
            self.compared,
            self.exhaustive_batches,
            self.production_batches,
            self.production_nonces,
            self.production_count_checks,
            if self.production_batches > 0 {
                self.production_nonces / self.production_batches as u64
            } else {
                0
            },
            self.production_reduction_checks,
            self.mismatches.len(),
            self.wall_seconds,
        ));
        text.push_str("  algorithm coverage (rounds executed, CPU-derived):\n");
        for (index, count) in self.algo_counts.iter().enumerate() {
            text.push_str(&format!(
                "    {:>2} {:<10} {:>12}{}\n",
                index,
                ALGO_NAMES[index],
                count,
                if *count == 0 { "   <-- NEVER TESTED" } else { "" }
            ));
        }
        for mismatch in self.mismatches.iter().take(20) {
            text.push_str(&mismatch.render());
            text.push('\n');
        }
        if self.mismatches.len() > 20 {
            text.push_str(&format!(
                "  ... and {} more mismatches\n",
                self.mismatches.len() - 20
            ));
        }
        text
    }
}

pub const ALGO_NAMES: [&str; 16] = [
    "blake", "bmw", "groestl", "jh", "keccak", "skein", "luffa", "cubehash", "shavite", "simd",
    "echo", "hamsi", "fugue", "shabal", "whirlpool", "sha512",
];

// ---------------------------------------------------------------------------
// Everything below needs a real device.
// ---------------------------------------------------------------------------

#[cfg(feature = "ocl")]
use crate::opencl_gpu::{OpenCLResources, SHARE_LIST_CAPACITY, block::do_group_block_mining_opencl_shares};

/// Launch shapes used by the exhaustive pass. Each one hashes exactly
/// `SHARE_LIST_CAPACITY` nonces so the share list returns the ENTIRE window,
/// while placing those nonces on the card differently: the counting sort in
/// `X16RS_RUN_REPEAT_LOOP` is per work group over `local_size * unit_size`
/// slots, so varying `unit_size` varies the ordering the kernel builds and the
/// contention on the histogram. A single shape would leave that untested.
#[cfg(feature = "ocl")]
pub const EXHAUSTIVE_SHAPES: [(u32, u32, u32); 3] = [
    // (work_groups, local_size, unit_size)  product must equal 1024
    (1, 256, 4),
    (2, 256, 2),
    (4, 256, 1),
];

/// The launch shape the rig actually mines with. Configurable so the gate can
/// be re-pointed when the tuning changes.
///
/// Available to `cfg(test)` as well as to the OpenCL build because the
/// auto-tuner's corpus arithmetic is built on it and has to be testable on a
/// machine with no GPU.
#[cfg(any(feature = "ocl", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub work_groups: u32,
    pub local_size: u32,
    pub unit_size: u32,
}

#[cfg(any(feature = "ocl", test))]
impl Shape {
    pub fn nonces(&self) -> u64 {
        self.work_groups as u64 * self.local_size as u64 * self.unit_size as u64
    }
}

/// Open one device with buffers sized for `unit_size`.
#[cfg(feature = "ocl")]
pub fn open_device(
    opencl_dir: &str,
    platform: u32,
    device: &str,
    shape: Shape,
) -> Result<OpenCLResources, String> {
    let scan = crate::opencl_diag::scan_opencl();
    let dir = opencl_dir.to_string();
    let devices = device.to_string();
    let mut resources = crate::opencl_gpu::initialize_opencl(
        false,
        &dir,
        &platform,
        &devices,
        &shape.work_groups,
        &shape.local_size,
        &shape.unit_size,
        Some(&scan),
        true,
    );
    if resources.is_empty() {
        return Err(format!(
            "no usable OpenCL device (platform {platform}, device_ids '{device}', dir '{opencl_dir}')"
        ));
    }
    if resources.len() != 1 {
        return Err(format!(
            "{} devices selected; the gate measures one device at a time",
            resources.len()
        ));
    }
    Ok(resources.remove(0))
}

/// Dump every hash the card produced for a window, using the pool share list
/// with the weakest possible target so that every nonce qualifies.
///
/// Returns the window in NONCE ORDER. Errors if the card did not return the
/// whole window, because a partial dump would silently shrink the gate.
#[cfg(feature = "ocl")]
fn gpu_dump_window(
    opencl: &OpenCLResources,
    height: u64,
    intro: &[u8],
    nonce_start: u32,
    shape: Shape,
) -> Result<Vec<[u8; 32]>, String> {
    let count = shape.nonces();
    if count as usize > SHARE_LIST_CAPACITY {
        return Err(format!(
            "shape hashes {count} nonces but the share list holds {SHARE_LIST_CAPACITY}; \
             an exhaustive dump needs work_groups * local_size * unit_size <= {SHARE_LIST_CAPACITY}"
        ));
    }
    let out = do_group_block_mining_opencl_shares(
        opencl,
        height,
        intro.to_vec(),
        nonce_start,
        shape.work_groups,
        shape.local_size,
        shape.unit_size,
        Some(&[0xffu8; 32]),
    )
    .map_err(|e| e.display())?;

    if out.share_hits != count {
        return Err(format!(
            "kernel counted {} hits for a {count}-nonce window; with target 0xff..ff every nonce \
             must qualify, so the kernel did not hash the window it was asked to",
            out.share_hits
        ));
    }
    if out.shares.len() as u64 != count {
        return Err(format!(
            "share list returned {} of {count} nonces",
            out.shares.len()
        ));
    }

    let mut window: Vec<Option<[u8; 32]>> = vec![None; count as usize];
    for (nonce, hash) in out.shares {
        let offset = nonce.wrapping_sub(nonce_start) as u64;
        if offset >= count {
            return Err(format!(
                "kernel returned nonce {nonce}, outside the window [{nonce_start}, {})",
                nonce_start.wrapping_add(count as u32)
            ));
        }
        if window[offset as usize].is_some() {
            return Err(format!("kernel returned nonce {nonce} twice"));
        }
        window[offset as usize] = Some(hash);
    }
    window
        .into_iter()
        .enumerate()
        .map(|(i, cell)| {
            cell.ok_or_else(|| format!("kernel never returned nonce {}", nonce_start as u64 + i as u64))
        })
        .collect()
}

/// Full byte-equivalence run.
///
/// `headers` corpus entries x `batches` exhaustive windows x every shape in
/// `EXHAUSTIVE_SHAPES` x every repeat in `GATE_HEIGHTS`, plus `prod_batches`
/// runs at the production shape.
#[cfg(feature = "ocl")]
#[allow(clippy::too_many_arguments)]
pub fn run_equivalence(
    opencl_dir: &str,
    platform: u32,
    device: &str,
    headers: u32,
    batches: u32,
    prod_shape: Shape,
    prod_batches: u32,
    prod_thresholds: u32,
    threads: usize,
) -> Result<EquivReport, String> {
    let started = Instant::now();
    let mut report = EquivReport::default();

    // Exhaustive pass. One device open per shape, because the GPU buffers are
    // sized from unit_size at init.
    for (work_groups, local_size, unit_size) in EXHAUSTIVE_SHAPES {
        let shape = Shape {
            work_groups,
            local_size,
            unit_size,
        };
        let opencl = open_device(opencl_dir, platform, device, shape)?;
        let count = shape.nonces() as u32;
        for height in GATE_HEIGHTS {
            let repeat = x16rs::block_hash_repeat(height);
            for header_index in 0..headers {
                let intro = corpus_header(header_index);
                for batch in 0..batches {
                    // A distinct nonce base per (shape, height, header, batch)
                    // so the gate never re-tests the same hashes twice.
                    let nonce_start = nonce_base(unit_size, height, header_index, batch);
                    let gpu = gpu_dump_window(&opencl, height, &intro, nonce_start, shape)?;
                    let cpu = cpu_hash_window(height, &intro, nonce_start, count, threads);
                    report.exhaustive_batches += 1;
                    compare_window(
                        &mut report,
                        height,
                        repeat,
                        header_index,
                        nonce_start,
                        &intro,
                        &gpu,
                        &cpu,
                    );
                }
            }
        }
        drop(opencl);
    }

    // Production-shape pass.
    //
    // A production launch hashes 589 824 nonces and the share list holds 1024,
    // so the whole window cannot be dumped. Sampling 1024 of them would leave
    // 99.83% of the shape the miner actually runs unchecked, which is not a
    // gate. Instead this pass uses the share COUNTER, which the kernel
    // increments for every hash in the window:
    //
    //   * the CPU hashes the entire window and sorts it;
    //   * for each of several rank thresholds k, the target is set to the k-th
    //     smallest CPU hash, so exactly k of the 589 824 must qualify;
    //   * the kernel is run and `share_hits` must equal k EXACTLY.
    //
    // Every one of those launches reads all 589 824 GPU hashes. A single
    // corrupted hash lands on the wrong side of a threshold at quantile q with
    // probability ~2q(1-q); spread the thresholds and one wrong hash anywhere
    // in the window is caught with high probability. On top of that, the run
    // whose threshold yields exactly SHARE_LIST_CAPACITY hits returns those
    // 1024 hashes in full, and every byte of them is compared.
    if prod_batches > 0 && prod_shape.nonces() > 0 {
        let opencl = open_device(opencl_dir, platform, device, prod_shape)?;
        let height = REPEAT16_HEIGHT;
        let repeat = x16rs::block_hash_repeat(height);
        let window = prod_shape.nonces();
        if window > u32::MAX as u64 {
            return Err("production window exceeds the 32-bit nonce space".to_string());
        }
        let ranks = threshold_ranks(window, SHARE_LIST_CAPACITY as u64, prod_thresholds);
        for batch in 0..prod_batches {
            let header_index = batch % headers.max(1);
            let intro = corpus_header(header_index);
            let nonce_start = 0x4000_0000u32.wrapping_add(batch.wrapping_mul(window as u32));

            // The oracle for the WHOLE window, not a sample.
            let cpu = cpu_hash_window(height, &intro, nonce_start, window as u32, threads);
            let mut sorted = cpu.clone();
            sorted.sort_unstable();

            report.production_batches += 1;
            report.production_nonces += window;

            for rank in ranks.iter().copied() {
                let target = sorted[(rank - 1) as usize];
                let out = do_group_block_mining_opencl_shares(
                    &opencl,
                    height,
                    intro.clone(),
                    nonce_start,
                    prod_shape.work_groups,
                    prod_shape.local_size,
                    prod_shape.unit_size,
                    Some(&target),
                )
                .map_err(|e| e.display())?;
                report.production_count_checks += 1;
                if out.share_hits != rank {
                    return Err(format!(
                        "production shape, header {header_index}, nonce base {nonce_start}: \
                         the CPU says exactly {rank} of the {window} hashes are <= {}, the kernel counted {}. \
                         The kernel's hashes differ from the CPU's somewhere in the window.",
                        hex::encode(target),
                        out.share_hits
                    ));
                }
                // Whatever did come back must be byte-exact and in-window.
                for (nonce, hash) in &out.shares {
                    let offset = nonce.wrapping_sub(nonce_start) as u64;
                    if offset >= window {
                        return Err(format!(
                            "production shape returned nonce {nonce}, outside the window"
                        ));
                    }
                    record(
                        &mut report,
                        height,
                        repeat,
                        header_index,
                        *nonce,
                        &intro,
                        hash,
                        &cpu[offset as usize],
                    );
                }
            }

            // The best-hash reduction is a separate code path from the share
            // list, and it is the ONLY path solo mining reads. It must return
            // the true minimum of the window, which the sorted oracle knows.
            let (best_nonce, best_hash) = crate::opencl_gpu::block::do_group_block_mining_opencl(
                &opencl,
                height,
                intro.clone(),
                nonce_start,
                prod_shape.work_groups,
                prod_shape.local_size,
                prod_shape.unit_size,
            )
            .map_err(|e| e.display())?;
            if best_hash != sorted[0] {
                return Err(format!(
                    "production shape best-hash reduction returned {} for nonce {best_nonce}; \
                     the CPU minimum over the window is {}",
                    hex::encode(best_hash),
                    hex::encode(sorted[0])
                ));
            }
            let best_offset = best_nonce.wrapping_sub(nonce_start) as u64;
            if best_offset >= window || cpu[best_offset as usize] != best_hash {
                return Err(format!(
                    "production shape best nonce {best_nonce} does not carry its own hash"
                ));
            }
            report.production_reduction_checks += 1;
        }
        drop(opencl);
    }

    report.wall_seconds = started.elapsed().as_secs_f64();
    Ok(report)
}

/// Nonce base that keeps every (shape, height, header, batch) window disjoint.
#[cfg(feature = "ocl")]
fn nonce_base(unit_size: u32, height: u64, header_index: u32, batch: u32) -> u32 {
    let height_slot = GATE_HEIGHTS
        .iter()
        .position(|h| *h == height)
        .unwrap_or(0) as u32;
    0x0100_0000u32
        .wrapping_mul(unit_size)
        .wrapping_add(0x0010_0000u32.wrapping_mul(height_slot))
        .wrapping_add(0x0000_8000u32.wrapping_mul(header_index))
        .wrapping_add(0x0000_0400u32.wrapping_mul(batch))
}

#[cfg(feature = "ocl")]
#[allow(clippy::too_many_arguments)]
fn compare_window(
    report: &mut EquivReport,
    height: u64,
    repeat: i32,
    header_index: u32,
    nonce_start: u32,
    intro: &[u8],
    gpu: &[[u8; 32]],
    cpu: &[[u8; 32]],
) {
    for (i, (g, c)) in gpu.iter().zip(cpu.iter()).enumerate() {
        let nonce = nonce_start.wrapping_add(i as u32);
        record(report, height, repeat, header_index, nonce, intro, g, c);
    }
}

#[cfg(feature = "ocl")]
#[allow(clippy::too_many_arguments)]
fn record(
    report: &mut EquivReport,
    height: u64,
    repeat: i32,
    header_index: u32,
    nonce: u32,
    intro: &[u8],
    gpu: &[u8; 32],
    cpu: &[u8; 32],
) {
    report.compared += 1;
    // Algorithm coverage is sampled, not counted for every nonce: deriving the
    // per-round algorithm costs `repeat` extra CPU hashes, which would double
    // the oracle's cost for a number that converges in a few hundred samples.
    if report.compared % 64 == 0 {
        for algo in algo_sequence(height, intro, nonce) {
            report.algo_counts[(algo & 0x0f) as usize] += 1;
        }
    }
    if gpu != cpu {
        if report.mismatches.len() < 4096 {
            report.mismatches.push(Mismatch {
                height,
                repeat,
                header_index,
                nonce,
                gpu: *gpu,
                cpu: *cpu,
                algos: algo_sequence(height, intro, nonce),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Fixed-work baseline
// ---------------------------------------------------------------------------

/// Middle-80% spread of an ascending sample, as a percentage of `centre`.
/// Robust to the one-in-a-dozen stalled run that peak-to-peak cannot survive.
#[cfg(feature = "ocl")]
pub fn spread_p10_p90(sorted: &[f64], centre: f64) -> f64 {
    if sorted.is_empty() || centre <= 0.0 {
        return 0.0;
    }
    let index = |q: f64| -> usize {
        let raw = (q * (sorted.len() as f64 - 1.0)).round() as usize;
        raw.min(sorted.len() - 1)
    };
    (sorted[index(0.9)] - sorted[index(0.1)]) / centre * 100.0
}

/// One timed run over the fixed corpus.
#[cfg(feature = "ocl")]
#[derive(Clone, Debug)]
pub struct BaselineRun {
    pub seconds: f64,
    pub nonces: u64,
    pub hashrate: f64,
}

#[cfg(feature = "ocl")]
#[derive(Clone, Debug)]
pub struct BaselineReport {
    pub shape: Shape,
    pub height: u64,
    pub repeat: i32,
    pub batches_per_run: u32,
    pub nonces_per_run: u64,
    pub nonce_start: u32,
    pub header_indices: Vec<u32>,
    pub runs: Vec<BaselineRun>,
    pub cpu_spot_checks: u32,
}

#[cfg(feature = "ocl")]
impl BaselineReport {
    fn sorted_rates(&self) -> Vec<f64> {
        let mut rates: Vec<f64> = self.runs.iter().map(|r| r.hashrate).collect();
        rates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        rates
    }
    pub fn median(&self) -> f64 {
        let rates = self.sorted_rates();
        if rates.is_empty() {
            return 0.0;
        }
        let mid = rates.len() / 2;
        if rates.len() % 2 == 0 {
            (rates[mid - 1] + rates[mid]) / 2.0
        } else {
            rates[mid]
        }
    }
    pub fn min(&self) -> f64 {
        self.sorted_rates().first().copied().unwrap_or(0.0)
    }
    pub fn max(&self) -> f64 {
        self.sorted_rates().last().copied().unwrap_or(0.0)
    }
    /// Peak-to-peak spread as a percentage of the median. THIS is the noise
    /// floor: a later change that moves the median by less than this has not
    /// been shown to do anything.
    pub fn spread_pct(&self) -> f64 {
        let median = self.median();
        if median <= 0.0 {
            return 0.0;
        }
        (self.max() - self.min()) / median * 100.0
    }

    pub fn render(&self) -> String {
        let mut text = format!(
            "  launch shape     : work_groups={} local_size={} unit_size={}  ({} nonces/batch)\n  \
             height           : {}  (x16rs repeat = {})\n  \
             fixed corpus     : {} batches/run, nonce range [{}, {}), headers {:?}\n  \
             identical work   : every run hashes the SAME {} nonces of the SAME headers\n  \
             CPU spot checks  : {} best-nonce hashes re-hashed with x16rs::block_hash, byte-equal\n  \
             runs             : {}\n",
            self.shape.work_groups,
            self.shape.local_size,
            self.shape.unit_size,
            self.shape.nonces(),
            self.height,
            self.repeat,
            self.batches_per_run,
            self.nonce_start,
            self.nonce_start as u64 + self.nonces_per_run,
            self.header_indices,
            self.nonces_per_run,
            self.cpu_spot_checks,
            self.runs.len(),
        );
        for (i, run) in self.runs.iter().enumerate() {
            text.push_str(&format!(
                "    run {:>2} : {:>8.3}s  {:>10}\n",
                i + 1,
                run.seconds,
                crate::bench_mainnet_repeat16::fmt_rate(run.hashrate)
            ));
        }
        text.push_str(&format!(
            "  median           : {}\n  min / max        : {} / {}\n  \
             within-process   : p10-p90 {:.2}%, peak-to-peak {:.2}%\n  \
             WARNING          : the within-process figure is NOT the bar for comparing two builds.\n                     \
             This card settles into one of several clock/power states for the life of a\n                     \
             process, and separate invocations of THIS command on identical work have\n                     \
             disagreed by ~2.6% on this rig. To compare two kernels, use `x16rs_gate ab`,\n                     \
             which alternates them inside one process and resolves ~0.3%.\n",
            crate::bench_mainnet_repeat16::fmt_rate(self.median()),
            crate::bench_mainnet_repeat16::fmt_rate(self.min()),
            crate::bench_mainnet_repeat16::fmt_rate(self.max()),
            spread_p10_p90(&self.sorted_rates(), self.median()),
            self.spread_pct(),
        ));
        text
    }
}

/// Fixed-work baseline: hash an identical, deterministic nonce range every run
/// and time it.
///
/// The corpus is fixed on purpose. x16rs picks a different algorithm chain for
/// every nonce, and the algorithms differ in cost by more than an order of
/// magnitude, so a run-for-N-seconds benchmark compares different work each
/// time and its variance is dominated by which hashes it happened to reach.
#[cfg(feature = "ocl")]
#[allow(clippy::too_many_arguments)]
pub fn run_baseline(
    opencl_dir: &str,
    platform: u32,
    device: &str,
    shape: Shape,
    height: u64,
    batches_per_run: u32,
    runs: u32,
    warmup_batches: u32,
    headers: u32,
) -> Result<BaselineReport, String> {
    let opencl = open_device(opencl_dir, platform, device, shape)?;
    let per_batch = shape.nonces();
    let nonce_start = 0x1000_0000u32;
    let headers = headers.max(1);
    let header_indices: Vec<u32> = (0..batches_per_run).map(|b| b % headers).collect();
    let intros: Vec<Vec<u8>> = (0..headers).map(corpus_header).collect();

    // Warm-up: clocks, JIT, caches. Outside the timed region and outside the
    // corpus, so the measured work is unchanged by how long the warm-up ran.
    for w in 0..warmup_batches {
        let start = 0xF000_0000u32.wrapping_add(w.wrapping_mul(per_batch as u32));
        crate::opencl_gpu::block::do_group_block_mining_opencl(
            &opencl,
            height,
            intros[0].clone(),
            start,
            shape.work_groups,
            shape.local_size,
            shape.unit_size,
        )
        .map_err(|e| format!("warm-up batch {}: {}", w + 1, e.display()))?;
    }

    let mut out_runs = Vec::with_capacity(runs as usize);
    let mut cpu_spot_checks = 0u32;
    for run in 0..runs {
        let started = Instant::now();
        let mut results: Vec<(u32, [u8; 32], u32)> = Vec::with_capacity(batches_per_run as usize);
        for batch in 0..batches_per_run {
            let header_index = header_indices[batch as usize];
            let start = nonce_start.wrapping_add(batch.wrapping_mul(per_batch as u32));
            let (nonce, hash) = crate::opencl_gpu::block::do_group_block_mining_opencl(
                &opencl,
                height,
                intros[header_index as usize].clone(),
                start,
                shape.work_groups,
                shape.local_size,
                shape.unit_size,
            )
            .map_err(|e| format!("run {} batch {}: {}", run + 1, batch + 1, e.display()))?;
            results.push((nonce, hash, header_index));
        }
        let seconds = started.elapsed().as_secs_f64();
        if !seconds.is_finite() || seconds <= 0.0 {
            return Err("non-positive run duration".to_string());
        }

        // Correctness is checked OUTSIDE the timed region so the check cannot
        // change the number. Every batch's best nonce is re-hashed on the CPU:
        // a timing run that measured a broken kernel is worthless.
        for (nonce, hash, header_index) in &results {
            let expect = cpu_hash(height, &intros[*header_index as usize], *nonce);
            if expect != *hash {
                return Err(format!(
                    "run {} produced a wrong hash at nonce {}: gpu={} cpu={}",
                    run + 1,
                    nonce,
                    hex::encode(hash),
                    hex::encode(expect)
                ));
            }
            cpu_spot_checks += 1;
        }

        let nonces = per_batch.saturating_mul(batches_per_run as u64);
        out_runs.push(BaselineRun {
            seconds,
            nonces,
            hashrate: nonces as f64 / seconds,
        });
    }

    Ok(BaselineReport {
        shape,
        height,
        repeat: x16rs::block_hash_repeat(height),
        batches_per_run,
        nonces_per_run: per_batch.saturating_mul(batches_per_run as u64),
        nonce_start,
        header_indices: (0..headers).collect(),
        runs: out_runs,
        cpu_spot_checks,
    })
}

// ---------------------------------------------------------------------------
// Paired A/B
// ---------------------------------------------------------------------------

/// One A-then-B pair, timed back to back on the same card in the same process.
#[cfg(feature = "ocl")]
#[derive(Clone, Debug)]
pub struct AbPair {
    pub a_seconds: f64,
    pub b_seconds: f64,
    /// B's hashrate divided by A's. > 1 means B is faster.
    pub ratio: f64,
}

#[cfg(feature = "ocl")]
#[derive(Clone, Debug)]
pub struct AbReport {
    pub dir_a: String,
    pub dir_b: String,
    pub shape: Shape,
    pub height: u64,
    pub batches_per_leg: u32,
    pub nonces_per_leg: u64,
    pub pairs: Vec<AbPair>,
    pub cpu_checks: u32,
    /// True when both legs produced identical hashes for identical work, which
    /// is the only condition under which comparing their speed means anything.
    pub identical_output: bool,
}

#[cfg(feature = "ocl")]
impl AbReport {
    fn sorted_ratios(&self) -> Vec<f64> {
        let mut r: Vec<f64> = self.pairs.iter().map(|p| p.ratio).collect();
        r.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        r
    }
    pub fn median_ratio(&self) -> f64 {
        let r = self.sorted_ratios();
        if r.is_empty() {
            return 0.0;
        }
        let mid = r.len() / 2;
        if r.len() % 2 == 0 {
            (r[mid - 1] + r[mid]) / 2.0
        } else {
            r[mid]
        }
    }
    /// Peak-to-peak spread of the PAIRED ratio.
    pub fn ratio_spread_pct(&self) -> f64 {
        let r = self.sorted_ratios();
        match (r.first(), r.last()) {
            (Some(lo), Some(hi)) if *lo > 0.0 => (hi - lo) / self.median_ratio() * 100.0,
            _ => 0.0,
        }
    }
    /// Middle-80% spread of the paired ratio. THIS is the resolution of an A/B
    /// comparison. Peak-to-peak is reported too, but it is dominated by the
    /// occasional OS or driver hitch: one stalled leg in a dozen turns a 0.3%
    /// spread into 7%, which would set an absurd bar and hide real wins.
    pub fn ratio_p10_p90_pct(&self) -> f64 {
        spread_p10_p90(&self.sorted_ratios(), self.median_ratio())
    }

    pub fn render(&self) -> String {
        let mut text = format!(
            "  A : {}\n  B : {}\n  \
             shape {}x{}x{} ({} nonces/batch), height {}, {} batches per leg ({} nonces)\n  \
             legs are ALTERNATED A,B,A,B... in one process, so both see the same clock state\n  \
             identical output : {}\n  CPU checks : {}\n",
            self.dir_a,
            self.dir_b,
            self.shape.work_groups,
            self.shape.local_size,
            self.shape.unit_size,
            self.shape.nonces(),
            self.height,
            self.batches_per_leg,
            self.nonces_per_leg,
            if self.identical_output {
                "yes - A and B returned byte-identical hashes for the same work"
            } else {
                "NO - the two builds do not agree; the speed comparison is meaningless"
            },
            self.cpu_checks,
        );
        for (i, pair) in self.pairs.iter().enumerate() {
            text.push_str(&format!(
                "    pair {:>2} : A {:>7.3}s  B {:>7.3}s  B/A = {:.4}\n",
                i + 1,
                pair.a_seconds,
                pair.b_seconds,
                pair.ratio
            ));
        }
        text.push_str(&format!(
            "  median B/A       : {:.4}  ({:+.2}%)\n  \
             paired p10-p90   : {:.2}%   <-- the resolution of this A/B test\n  \
             paired peak-peak : {:.2}%   (inflated by any single OS/driver hitch)\n",
            self.median_ratio(),
            (self.median_ratio() - 1.0) * 100.0,
            self.ratio_p10_p90_pct(),
            self.ratio_spread_pct(),
        ));
        text
    }
}

/// Paired A/B between two kernel trees on the same card, in one process.
///
/// Why paired. Two separate baseline processes on this rig disagree by ~2.5%
/// even on identical work, because the card settles into one of a couple of
/// clock/power states and stays there for the life of the process. That drift
/// is larger than most kernel changes worth making, so comparing the medians of
/// two separate runs cannot resolve them. Alternating the two builds inside one
/// process puts both legs in the same power state within seconds of each other,
/// and the ratio cancels the drift.
///
/// Run it with `dir_b == dir_a` first. The median ratio must be 1.000 and the
/// paired spread is then the floor of the method itself.
#[cfg(feature = "ocl")]
#[allow(clippy::too_many_arguments)]
pub fn run_ab(
    dir_a: &str,
    dir_b: &str,
    platform: u32,
    device: &str,
    shape: Shape,
    height: u64,
    batches_per_leg: u32,
    pairs: u32,
    warmup_batches: u32,
    headers: u32,
) -> Result<AbReport, String> {
    let a = open_device(dir_a, platform, device, shape)?;
    let b = open_device(dir_b, platform, device, shape)?;
    let per_batch = shape.nonces();
    let headers = headers.max(1);
    let intros: Vec<Vec<u8>> = (0..headers).map(corpus_header).collect();
    let nonce_start = 0x1000_0000u32;

    let leg = |res: &OpenCLResources| -> Result<(f64, Vec<(u32, [u8; 32], u32)>), String> {
        let started = Instant::now();
        let mut out = Vec::with_capacity(batches_per_leg as usize);
        for batch in 0..batches_per_leg {
            let header_index = batch % headers;
            let start = nonce_start.wrapping_add(batch.wrapping_mul(per_batch as u32));
            let (nonce, hash) = crate::opencl_gpu::block::do_group_block_mining_opencl(
                res,
                height,
                intros[header_index as usize].clone(),
                start,
                shape.work_groups,
                shape.local_size,
                shape.unit_size,
            )
            .map_err(|e| e.display())?;
            out.push((nonce, hash, header_index));
        }
        Ok((started.elapsed().as_secs_f64(), out))
    };

    for w in 0..warmup_batches {
        let start = 0xF000_0000u32.wrapping_add(w.wrapping_mul(per_batch as u32));
        let res = if w % 2 == 0 { &a } else { &b };
        crate::opencl_gpu::block::do_group_block_mining_opencl(
            res,
            height,
            intros[0].clone(),
            start,
            shape.work_groups,
            shape.local_size,
            shape.unit_size,
        )
        .map_err(|e| format!("warm-up batch {}: {}", w + 1, e.display()))?;
    }

    let mut out_pairs = Vec::with_capacity(pairs as usize);
    let mut cpu_checks = 0u32;
    let mut identical_output = true;
    for pair in 0..pairs {
        // Alternate which leg goes first. Running A first every time gave a
        // reproducible +0.11% edge to B on this rig when both trees were
        // identical, i.e. a bias of the method, not of the kernel. Swapping the
        // order on alternate pairs cancels it instead of leaving it to be
        // mistaken for a 0.1% win.
        let (a_seconds, a_out, b_seconds, b_out) = if pair % 2 == 0 {
            let (a_seconds, a_out) = leg(&a)?;
            let (b_seconds, b_out) = leg(&b)?;
            (a_seconds, a_out, b_seconds, b_out)
        } else {
            let (b_seconds, b_out) = leg(&b)?;
            let (a_seconds, a_out) = leg(&a)?;
            (a_seconds, a_out, b_seconds, b_out)
        };
        if a_seconds <= 0.0 || b_seconds <= 0.0 {
            return Err("non-positive leg duration".to_string());
        }
        // Same work, so the two legs must return the same answers. A speed
        // comparison between builds that disagree is worthless.
        if a_out.len() != b_out.len()
            || a_out
                .iter()
                .zip(b_out.iter())
                .any(|(x, y)| x.0 != y.0 || x.1 != y.1)
        {
            identical_output = false;
        }
        for (nonce, hash, header_index) in &a_out {
            if cpu_hash(height, &intros[*header_index as usize], *nonce) != *hash {
                return Err(format!("leg A returned a wrong hash at nonce {nonce}"));
            }
            cpu_checks += 1;
        }
        out_pairs.push(AbPair {
            a_seconds,
            b_seconds,
            ratio: a_seconds / b_seconds,
        });
    }

    Ok(AbReport {
        dir_a: dir_a.to_string(),
        dir_b: dir_b.to_string(),
        shape,
        height,
        batches_per_leg,
        nonces_per_leg: per_batch.saturating_mul(batches_per_leg as u64),
        pairs: out_pairs,
        cpu_checks,
        identical_output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_is_deterministic_and_respects_the_kernel_padding() {
        for index in 0..8 {
            let a = corpus_header(index);
            let b = corpus_header(index);
            assert_eq!(a, b, "corpus header {index} must be reproducible");
            assert_eq!(a.len(), BLOCK_INTRO_BYTES);
            assert_eq!(
                a[BLOCK_INTRO_BYTES - 1],
                0,
                "byte 88 must be zero or the card and the CPU hash different messages"
            );
        }
        assert_ne!(corpus_header(0), corpus_header(1));
    }

    #[test]
    fn gate_heights_cover_the_intended_repeats() {
        let repeats: Vec<i32> = GATE_HEIGHTS
            .iter()
            .map(|h| x16rs::block_hash_repeat(*h))
            .collect();
        assert_eq!(repeats, vec![1, 4, 8, 16]);
    }

    #[test]
    fn algo_sequence_has_one_entry_per_round_and_matches_the_first_pick() {
        let intro = corpus_header(3);
        let seq = algo_sequence(REPEAT16_HEIGHT, &intro, 12_345);
        assert_eq!(seq.len(), 16);
        assert!(seq.iter().all(|a| *a < 16));
        // Round 0 acts on the sha3 seed itself.
        let mut stuff = intro.clone();
        stuff[NONCE_OFFSET..NONCE_OFFSET + 4].copy_from_slice(&12_345u32.to_be_bytes());
        let seed = x16rs::calculate_hash(&stuff);
        assert_eq!(seq[0], seed[28] & 0x0f);
    }

    /// The oracle's price list, and the fact that the measurement behind it is
    /// close enough to what this build really does.
    ///
    /// `CPU_ORACLE_HPS_PER_CORE` is a measured number that the auto-tuner quotes
    /// to operators, so a build where it is wrong by an order of magnitude has
    /// to fail here rather than in someone's log. The note this replaced said
    /// "a few hundred hashes a second per core", which is wrong by more than two
    /// hundred times in a release build and by fifty in a debug one; being that
    /// wrong about the oracle is what makes a proof look impossible when it
    /// takes four seconds.
    ///
    /// The check on the live rate is a factor-of-two band, not an equality: this
    /// runs on whatever machine the gate runs on, usually while that machine is
    /// mining. A factor of two still catches the failure that actually happens,
    /// which is a constant nobody re-measured after the kernel changed.
    #[test]
    fn the_oracle_costs_what_the_tuner_says_it_costs() {
        // 64x256x192, the largest launch an RX 9070 XT can be asked for.
        let (seconds, bytes) = oracle_cost(3_145_728, 14);
        assert!(seconds > 3.5 && seconds < 4.0, "{seconds}");
        assert_eq!(bytes, 3_145_728 * 64);
        // A 3072x256x128 launch, which a 3584 work-group preset permits: thirty
        // times the work and gigabytes of it.
        let (seconds, bytes) = oracle_cost(100_663_296, 14);
        assert!(seconds > 110.0 && seconds < 130.0, "{seconds}");
        assert_eq!(bytes / (1024 * 1024), 6_144, "{bytes} bytes");
        // Threads help, and a nonsense thread count does not divide by zero.
        assert!(oracle_cost(1_000_000, 0).0 > oracle_cost(1_000_000, 8).0);
        assert!(oracle_cost(0, 4).0 == 0.0);

        // 2 000 nonces: 26 ms in release, 123 ms in debug, so this is affordable
        // in the build the gate really runs and long enough to be past the
        // timer's resolution and the first-call warm-up.
        let intro = corpus_header(7);
        let count = 2_000u32;
        let started = std::time::Instant::now();
        let out = cpu_hash_window(REPEAT16_HEIGHT, &intro, 1_000, count, 1);
        let measured = count as f64 / started.elapsed().as_secs_f64();
        assert_eq!(out.len(), count as usize);
        assert!(
            measured > 1_000.0,
            "one core managed {measured:.0} H/s, which is not a working oracle"
        );
        if !cfg!(debug_assertions) {
            assert!(
                measured > CPU_ORACLE_HPS_PER_CORE / 2.0
                    && measured < CPU_ORACLE_HPS_PER_CORE * 2.0,
                "a release build measures {measured:.0} H/s against the \
                 {CPU_ORACLE_HPS_PER_CORE:.0} this constant promises; every proof estimate the \
                 tuner prints is out by that factor"
            );
        }
    }

    #[test]
    fn threshold_ranks_are_in_range_and_include_a_full_list() {
        let window = 589_824u64;
        let ranks = threshold_ranks(window, 1024, 255);
        assert!(ranks.contains(&1024), "one launch must fill the share list");
        assert!(ranks.windows(2).all(|w| w[0] < w[1]), "sorted and deduped");
        assert!(ranks.iter().all(|r| *r >= 1 && *r <= window));
        // One wrong hash anywhere in the window has to be unlikely to slip past
        // the whole threshold set, or the production pass is decoration.
        let miss = threshold_miss_probability(window, &ranks);
        assert!(miss < 0.005, "miss probability {miss} is too high");
        // More thresholds must strictly help, and the relationship is ~1/levels.
        assert!(
            threshold_miss_probability(window, &threshold_ranks(window, 1024, 1023)) < miss / 3.0
        );
    }

    #[test]
    fn threaded_oracle_equals_the_single_threaded_one() {
        let intro = corpus_header(1);
        let single = cpu_hash_window(1, &intro, 900, 40, 1);
        let many = cpu_hash_window(1, &intro, 900, 40, 8);
        assert_eq!(single, many);
    }
}
