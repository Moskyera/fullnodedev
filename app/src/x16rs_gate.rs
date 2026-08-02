//! x16rs equivalence gate and fixed-corpus baseline.
//!
//! ADDITIVE MODULE. Nothing in the mining path calls it; it exists so that any
//! future change to a GPU kernel can be judged against two things that a
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
//! ONE GATE, TWO BACKENDS. The corpus, the CPU oracle, the exhaustive-window
//! reassembly, the threshold arithmetic, the comparison, the algorithm coverage
//! counting and the blame attribution live once, in code that is not compiled
//! per backend. OpenCL and CUDA each supply only three device operations
//! ([`GateDevice`]) and a way to open a device at a given launch shape
//! ([`GateBackend`]). A gate that had been forked per backend would drift, and
//! the fork would be discovered by one of them passing something the other
//! catches.
//!
//! How `equiv` gets every hash off the card. The mining kernel returns only the
//! best hash per work group, which would prove one hash in a hundred thousand.
//! The pool share list (`x16rs_main.cl`, and the identical block in
//! `x16rs-cuda/cuda/block_miner.cu`, both keyed on `share_capacity != 0`)
//! already emits (nonce, hash) for every nonce that beats a target. With target
//! 0xff..ff every nonce qualifies, so a batch sized to exactly the share list's
//! capacity dumps its ENTIRE window. That is the exhaustive mode, and it is
//! available on BOTH backends today: the CUDA port carries the same 1024-entry
//! list, the same total-hit counter and the same append-before-reduce ordering.
//! The production launch shape (48 x 256 x 48 = 589 824 nonces) cannot fit in
//! the list, so there the gate checks the 1024 returned hashes byte for byte AND
//! checks that the kernel counted the whole window against a set of rank
//! thresholds.

#[cfg(any(feature = "ocl", feature = "cuda"))]
use std::time::Instant;

/// Height whose `block_hash_repeat` is the mainnet maximum, 16.
pub const REPEAT16_HEIGHT: u64 = 800_000;

/// How far apart two SEPARATE invocations of this binary land on identical work.
///
/// Measured on this rig by running `baseline` repeatedly over the fixed corpus:
/// within one process the runs reproduce to a few tenths of a percent, but the
/// card settles into one of several clock and power states for the LIFE of a
/// process, so two processes on the same work have disagreed by about this much.
///
/// It is the bar for any claim that compares a number from one run against a
/// number from another run - a different build, a different day, a different
/// kernel - and it is stated once here because two things now quote it: the
/// baseline report, and the auto-tuner, whose CUDA path cannot fall back to the
/// in-process A/B that resolves ten times finer (`run_ab` is OpenCL-only, and
/// structurally so: nvcc compiles CUDA kernels into the binary).
pub const BETWEEN_PROCESS_SPREAD_PCT: f64 = 2.6;

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
        let named: Vec<String> = self
            .algos
            .iter()
            .map(|a| format!("{}:{}", a, ALGO_NAMES[(*a & 0x0f) as usize]))
            .collect();
        format!(
            "  MISMATCH height={} repeat={} header={} nonce={}\n    gpu={}\n    cpu={}\n    cpu algo chain={}",
            self.height,
            self.repeat,
            self.header_index,
            self.nonce,
            hex::encode(self.gpu),
            hex::encode(self.cpu),
            named.join(" -> "),
        )
    }
}

/// Which of the sixteen algorithms the mismatches point at.
///
/// The reasoning is the only one the data actually supports, and it is worth
/// stating because a plausible-looking alternative is wrong. Each nonce runs a
/// chain of `repeat` algorithms chosen by its own hash. If exactly one algorithm
/// is broken then, necessarily:
///
///   * EVERY mismatching nonce ran it, so it survives the intersection of all
///     failing chains;
///   * NO matching nonce ran it, because running it would have produced a wrong
///     hash, so it appears in none of the sampled passing chains.
///
/// How much each half is worth, measured rather than assumed, by breaking each
/// of the sixteen algorithms in turn on this corpus at repeat 16:
///
///   * past about eight failing chains the intersection alone is usually already
///     a single algorithm, and a real failing run is nowhere near that margin -
///     one broken algorithm at repeat 16 corrupts 1 - (15/16)^16 = 64% of every
///     window, so a default `equiv` fails thousands of nonces, not eight;
///   * between three and eight it is not. With three failing chains, breaking
///     skein left {groestl, jh, keccak, skein, cubehash, fugue} in the
///     intersection and breaking luffa left seven candidates; the passing-chain
///     filter cut both to the one true culprit.
///
/// So the intersection carries a well-populated run and the passing filter
/// carries the thin one. Both are one pass over data the gate already has, and
/// dropping either would cost accuracy in exactly the case where the operator
/// most needs a name.
///
/// A defect that is NOT one algorithm - a missing barrier, a bad shared-table
/// fill, a wrong nonce write - leaves no algorithm in every failing chain, and
/// this reports exactly that instead of inventing a culprit. That is the honest
/// outcome for the one of the three injected OpenCL faults that could not be
/// named.
#[derive(Clone, Debug, Default)]
pub struct AlgoAttribution {
    /// Algorithms in every failing chain AND in no sampled passing chain.
    pub named: Vec<u8>,
    /// Algorithms in every failing chain, before the passing-chain filter.
    pub in_every_failure: Vec<u8>,
    /// Failing chains that ran each algorithm.
    pub failing: [u64; 16],
    /// Sampled passing chains that ran each algorithm.
    pub passing: [u64; 16],
    pub failing_chains: u64,
    pub passing_chains: u64,
}

/// Failing chains below which the gate refuses to name anything.
///
/// An innocent algorithm survives the intersection of n failing chains with
/// probability about 0.64^n at repeat 16, which is 3% at n = 8, and it then has
/// to be absent from every passing chain as well before a wrong name is printed.
/// Measured on this corpus: every case that reached eight failing chains named
/// the true culprit alone, while at five chains shavite, hamsi and shabal were
/// indistinguishable from each other and all three came back as a tie. So under
/// 8 the gate prints candidates instead. A confident wrong name would send
/// someone to read the wrong kernel file for an afternoon, and the list costs
/// them nothing.
pub const MIN_CHAINS_TO_NAME: u64 = 8;

impl AlgoAttribution {
    pub fn render(&self) -> String {
        if self.failing_chains == 0 {
            return String::new();
        }
        let mut text = String::new();
        let names = |set: &[u8]| -> String {
            set.iter()
                .map(|a| format!("{} ({})", ALGO_NAMES[(*a & 0x0f) as usize], a))
                .collect::<Vec<_>>()
                .join(", ")
        };
        text.push_str(&format!(
            "  blame: {} failing chains, {} sampled passing chains\n",
            self.failing_chains, self.passing_chains
        ));
        if self.failing_chains < MIN_CHAINS_TO_NAME {
            text.push_str(&format!(
                "    too few failing chains to name an algorithm (need {MIN_CHAINS_TO_NAME}); \
                 present in all of them: {}\n",
                names(&self.in_every_failure)
            ));
        } else if self.named.len() == 1 {
            text.push_str(&format!(
                "    ALGORITHM: {}. It ran in every one of the {} failing chains and in none \
                 of the {} passing ones.\n",
                names(&self.named),
                self.failing_chains,
                self.passing_chains
            ));
        } else if self.named.is_empty() {
            text.push_str(
                "    NO single algorithm is implicated: the failing nonces do not share one. \
                 That is what a race, a barrier, a shared-table fill or the nonce write looks \
                 like, not one algorithm function.\n",
            );
        } else {
            text.push_str(&format!(
                "    candidates (in every failure, in no passing chain): {}\n",
                names(&self.named)
            ));
        }
        text
    }
}

/// Totals for one full `equiv` run.
/// Marker that an error describes the KERNEL'S OUTPUT being wrong rather than a
/// failure to run.
///
/// It is a tag, not a phrase to be matched by guesswork. The wrapper scripts
/// used to map every error to "the gate could not open the device, nothing was
/// compared", which is what an operator was told on a run where the gate had
/// just caught the exact fault it exists to catch. The exhaustive shapes are
/// small, so a defect that only appears at the production shape reaches the
/// operator through one of these messages and nowhere else.
pub const DETECTED: &str = "[detected] ";

#[derive(Clone, Debug, Default)]
pub struct EquivReport {
    /// Which backend produced this. Free text so the report cannot be mistaken
    /// for the other card's.
    pub backend: String,
    /// The device the backend actually opened.
    pub device: String,
    pub compared: u64,
    pub mismatches: Vec<Mismatch>,
    /// How many times each of the 16 algorithms was actually executed by the
    /// compared corpus, according to the CPU. A zero here means the gate never
    /// tested that algorithm, which is a hole in the gate, not a pass.
    pub algo_counts: [u64; 16],
    /// Sampled chains of nonces the card got RIGHT, per algorithm, counted once
    /// per chain. The denominator of the attribution above.
    pub passing_algo_chains: [u64; 16],
    pub passing_chain_samples: u64,
    pub exhaustive_batches: u32,
    pub production_batches: u32,
    /// What the caller ASKED for, so a pass can be checked against the work that
    /// was requested rather than only against the work that happened.
    ///
    /// `--headers 0` and `--batches 0` each make the exhaustive loop body never
    /// run. The device is still opened, the production pass still satisfies
    /// `compared > 0` and every algorithm count, and the gate printed PASS with
    /// zero bytes compared exhaustively. The byte-for-byte pass is the whole
    /// reason this exists, so a flag must not be able to delete it quietly.
    pub asked_exhaustive: bool,
    pub asked_production: bool,
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
            && self.ran_what_was_asked()
    }

    /// Every pass the caller requested actually executed at least once.
    ///
    /// Without this, `--headers 0` or `--batches 0` removes the exhaustive
    /// byte-for-byte comparison and the gate still exits 0, because the
    /// production pass alone satisfies every other condition. A green gate that
    /// compared nothing is worse than a red one: it is believed.
    pub fn ran_what_was_asked(&self) -> bool {
        (!self.asked_exhaustive || self.exhaustive_batches > 0)
            && (!self.asked_production || self.production_batches > 0)
    }

    /// Why `passed()` is false, in the operator's words rather than a bool.
    pub fn failure_reason(&self) -> Option<String> {
        if !self.mismatches.is_empty() {
            return Some(format!("{} hashes differ from the CPU", self.mismatches.len()));
        }
        if self.compared == 0 {
            return Some("nothing was compared".to_string());
        }
        if let Some(algo) = self.algo_counts.iter().position(|count| *count == 0) {
            return Some(format!(
                "algorithm {algo} was never exercised, so the gate did not test it"
            ));
        }
        if self.asked_exhaustive && self.exhaustive_batches == 0 {
            return Some(
                "the exhaustive byte-for-byte pass was requested but ran zero batches; \
                 check --headers and --batches"
                    .to_string(),
            );
        }
        if self.asked_production && self.production_batches == 0 {
            return Some(
                "the production-shape pass was requested but ran zero batches; \
                 check --prod-batches"
                    .to_string(),
            );
        }
        None
    }

    /// Name the broken algorithm when the mismatches allow it. See
    /// [`AlgoAttribution`] for why both halves of the test are needed.
    pub fn attribute(&self) -> AlgoAttribution {
        let mut out = AlgoAttribution {
            passing: self.passing_algo_chains,
            passing_chains: self.passing_chain_samples,
            failing_chains: self.mismatches.len() as u64,
            ..Default::default()
        };
        for mismatch in &self.mismatches {
            // Once per chain, not once per round: a chain that runs shabal three
            // times is still one failing nonce, and counting rounds would make a
            // frequently repeated algorithm look guilty.
            let mut seen = [false; 16];
            for algo in &mismatch.algos {
                seen[(*algo & 0x0f) as usize] = true;
            }
            for (index, hit) in seen.iter().enumerate() {
                if *hit {
                    out.failing[index] += 1;
                }
            }
        }
        if out.failing_chains == 0 {
            return out;
        }
        for index in 0..16u8 {
            if out.failing[index as usize] == out.failing_chains {
                out.in_every_failure.push(index);
                if out.passing[index as usize] == 0 {
                    out.named.push(index);
                }
            }
        }
        out
    }

    pub fn render(&self) -> String {
        let mut text = String::new();
        text.push_str(&format!(
            "  backend / device : {} / {}\n",
            if self.backend.is_empty() {
                "?"
            } else {
                &self.backend
            },
            if self.device.is_empty() {
                "?"
            } else {
                &self.device
            },
        ));
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
        text.push_str(&self.attribute().render());
        for mismatch in self.mismatches.iter().take(20) {
            text.push_str(&mismatch.render());
            text.push('\n');
        }
        if self.mismatches.len() > 20 {
            text.push_str(&format!(
                "  ... and {} more mismatches (only the first {} are stored)\n",
                self.mismatches.len() - 20,
                MAX_STORED_MISMATCHES
            ));
        }
        text
    }
}

/// Mismatches kept in full. Past this the run has failed many times over and the
/// rest add nothing but memory; the blame attribution is already saturated.
pub const MAX_STORED_MISMATCHES: usize = 4096;

pub const ALGO_NAMES: [&str; 16] = [
    "blake", "bmw", "groestl", "jh", "keccak", "skein", "luffa", "cubehash", "shavite", "simd",
    "echo", "hamsi", "fugue", "shabal", "whirlpool", "sha512",
];

/// Entries the pool share list holds, on both backends.
///
/// `app/src/opencl_gpu/resources.rs` and `x16rs-cuda/src/lib.rs` each define
/// their own; this is the number the gate's exhaustive window is sized from, and
/// `run_equivalence_on` refuses to start if a backend disagrees with it, so the
/// two cannot drift apart silently.
pub const SHARE_LIST_CAPACITY: u64 = 1024;

/// Launch shapes used by the exhaustive pass. Each one hashes exactly
/// `SHARE_LIST_CAPACITY` nonces so the share list returns the ENTIRE window,
/// while placing those nonces on the card differently: the counting sort in
/// `X16RS_RUN_REPEAT_LOOP` is per work group over `local_size * unit_size`
/// slots, so varying `unit_size` varies the ordering the kernel builds and the
/// contention on the histogram. A single shape would leave that untested.
///
/// All three hold `local_size` at 256. That is not a preference: the CUDA host
/// fixes the block size at `x16rs_cuda::DEFAULT_LOCAL_SIZE` because
/// `block_miner.cu` declares `__shared__ unsigned int local_nonces[256]` and
/// reduces over a power-of-two tree, so 256 is the only block size that kernel
/// is correct at. Since the OpenCL gate already used 256 throughout, the two
/// backends run the identical three shapes and their reports are comparable.
pub const EXHAUSTIVE_SHAPES: [(u32, u32, u32); 3] = [
    // (work_groups, local_size, unit_size)  product must equal SHARE_LIST_CAPACITY
    (1, 256, 4),
    (2, 256, 2),
    (4, 256, 1),
];

/// The launch shape the rig actually mines with. Configurable so the gate can
/// be re-pointed when the tuning changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub work_groups: u32,
    pub local_size: u32,
    pub unit_size: u32,
}

impl Shape {
    pub fn nonces(&self) -> u64 {
        self.work_groups as u64 * self.local_size as u64 * self.unit_size as u64
    }
}

impl std::fmt::Display for Shape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}x{}x{}",
            self.work_groups, self.local_size, self.unit_size
        )
    }
}

/// Whether a device allocated for `allocated` may be LAUNCHED at `wanted`, on a
/// backend that takes its launch shape per call.
///
/// That is OpenCL: `do_group_block_mining_opencl` receives work_groups,
/// local_size and unit_size on every call, so one allocation serves every
/// smaller shape - which is what lets the auto-tuner measure forty candidates
/// against one set of buffers. Larger is refused rather than clamped:
/// `initialize_opencl` sizes `global_hashes` and `global_order` from the shape
/// it was opened with and the kernel indexes them by the shape it is LAUNCHED
/// with, so a larger launch writes past the buffers, which on a GPU is not a
/// crash but a wrong answer somewhere else.
///
/// Free-standing and feature-free so the rule can be tested without a card.
pub fn launch_fits_allocation(allocated: Shape, wanted: Shape) -> Result<(), String> {
    if wanted.local_size != allocated.local_size
        || wanted.work_groups > allocated.work_groups
        || wanted.unit_size > allocated.unit_size
    {
        return Err(format!(
            "launch shape {wanted} does not fit a device allocated for {allocated}: this backend \
             may be launched at any SMALLER shape with the same local_size, never a larger one"
        ));
    }
    Ok(())
}

/// The same question on a backend that bakes `unit_size` into the allocation AND
/// hands it to the kernel from there.
///
/// That is CUDA: `mine_batch_inner` passes `miner.unit_size`, so a miner built
/// at 64 asked for 128 does not fail - it runs 64 nonces per thread, returns
/// correct hashes for 64, and the caller labels the result 128. Nothing
/// downstream could catch that: the hashes are right, the equivalence proof
/// passes, and only the TIME is attributed to the wrong shape. So unit_size must
/// match exactly. Work groups are clamped per launch by
/// `mine_block_batch_shares`, so fewer is a real launch of fewer and more is
/// refused for the same reason a mislabelled unit_size is.
pub fn launch_fits_bound_allocation(allocated: Shape, wanted: Shape) -> Result<(), String> {
    if wanted.unit_size != allocated.unit_size || wanted.local_size != allocated.local_size {
        return Err(format!(
            "launch shape {wanted} cannot run on a device built for {allocated}: unit_size and \
             local_size are fixed at allocation and passed to the kernel from it, so this launch \
             would run unit_size {} and be reported as {}",
            allocated.unit_size, wanted.unit_size
        ));
    }
    if wanted.work_groups > allocated.work_groups {
        return Err(format!(
            "launch shape {wanted} asks for more work groups than were allocated ({allocated})"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Everything below needs a real device.
// ---------------------------------------------------------------------------

/// One opened device, and the launch shapes it can be asked for.
///
/// Three operations, and they are the only thing a backend has to supply. Every
/// piece of judgement - what to hash, what to compare it with, what counts as a
/// pass, which algorithm to blame - is above this line and is compiled once.
///
/// The launch shape is a PARAMETER of each operation rather than a property of
/// the device, because the two are not the same thing and the difference is the
/// whole reason the auto-tuner can share this trait. A device is opened with
/// buffers sized for one shape ([`GateDevice::allocated_shape`]); an OpenCL
/// device can then be launched at any smaller shape against those same buffers,
/// which is how the tuner measures forty candidates without recompiling a kernel
/// forty times, while a CUDA device cannot, because `unit_size` is baked into
/// its allocation AND passed to the kernel from the miner. Each backend says
/// which it is through [`GateBackend::device_is_bound_to_its_shape`], and each
/// device refuses a shape it cannot honestly launch instead of quietly running a
/// different one.
#[cfg(any(feature = "ocl", feature = "cuda"))]
pub trait GateDevice {
    /// The shape this device's buffers were sized for.
    fn allocated_shape(&self) -> Shape;

    /// True when this already-open device can launch `shape` honestly: not
    /// beyond its allocation, and not by quietly running a different shape.
    ///
    /// The auto-tuner asks before every candidate. Answering it wrongly in the
    /// permissive direction is the failure that would not announce itself: a
    /// CUDA miner asked for a `unit_size` it was not built with runs the one it
    /// WAS built with, returns correct hashes for that shape, passes every
    /// equivalence proof, and hands back a time the tuner would attribute to the
    /// shape it asked for. Each implementation answers from the same rule its
    /// launch path enforces, so the two cannot drift.
    fn can_launch(&self, shape: Shape) -> bool;

    /// Launch at `shape` from `nonce_start` with `share_target`, and return
    /// (total hits the kernel counted, the entries that fit in the share list).
    ///
    /// The total is the KERNEL's own count, including hits it could not store.
    /// That is what makes the production pass possible: it is a statement about
    /// every hash in the window, not about the ones that fit down the pipe.
    fn count_and_shares(
        &self,
        shape: Shape,
        height: u64,
        intro: &[u8],
        nonce_start: u32,
        share_target: &[u8; 32],
    ) -> Result<(u64, Vec<(u32, [u8; 32])>), String>;

    /// The best-hash reduction: one nonce and hash for the whole window. This is
    /// a different code path from the share list and it is the ONLY one solo
    /// mining reads, so the gate proves it separately.
    fn best(
        &self,
        shape: Shape,
        height: u64,
        intro: &[u8],
        nonce_start: u32,
    ) -> Result<(u32, [u8; 32]), String>;

    /// Every hash the card produced for a window, in NONCE ORDER.
    ///
    /// Provided, not per backend: this is the trick the whole exhaustive mode
    /// rests on (weakest possible target, so every nonce qualifies and the share
    /// list becomes a full dump), and having it in one place is what stops the
    /// two backends proving subtly different things. Errors rather than
    /// truncates if the card did not return the whole window, because a partial
    /// dump would silently shrink the gate to a sample.
    fn dump_window(
        &self,
        shape: Shape,
        height: u64,
        intro: &[u8],
        nonce_start: u32,
    ) -> Result<Vec<[u8; 32]>, String> {
        let count = shape.nonces();
        let (hits, shares) =
            self.count_and_shares(shape, height, intro, nonce_start, &[0xffu8; 32])?;
        if hits != count {
            return Err(format!(
                "{DETECTED}kernel counted {hits} hits for a {count}-nonce window; with target 0xff..ff every \
                 nonce must qualify, so the kernel did not hash the window it was asked to"
            ));
        }
        if shares.len() as u64 != count {
            return Err(format!(
                "{DETECTED}share list returned {} of {count} nonces",
                shares.len()
            ));
        }
        let mut window: Vec<Option<[u8; 32]>> = vec![None; count as usize];
        for (nonce, hash) in shares {
            let offset = nonce.wrapping_sub(nonce_start) as u64;
            if offset >= count {
                return Err(format!(
                    "{DETECTED}kernel returned nonce {nonce}, outside the window [{nonce_start}, {})",
                    nonce_start.wrapping_add(count as u32)
                ));
            }
            if window[offset as usize].is_some() {
                return Err(format!("{DETECTED}kernel returned nonce {nonce} twice"));
            }
            window[offset as usize] = Some(hash);
        }
        window
            .into_iter()
            .enumerate()
            .map(|(i, cell)| {
                cell.ok_or_else(|| {
                    format!("kernel never returned nonce {}", nonce_start as u64 + i as u64)
                })
            })
            .collect()
    }
}

/// A way to open devices. One per GPU API.
#[cfg(any(feature = "ocl", feature = "cuda"))]
pub trait GateBackend {
    type Device: GateDevice;

    /// What the report calls this backend.
    fn name(&self) -> &'static str;

    /// The device under test, named well enough to identify it in a log.
    fn describe(&self) -> String;

    /// Entries this backend's share list holds. Checked against
    /// [`SHARE_LIST_CAPACITY`] before anything runs.
    fn share_capacity(&self) -> u64;

    /// Reject a shape this backend cannot launch, before a device is opened and
    /// before the CPU spends minutes hashing an oracle for it.
    fn check_shape(&self, shape: Shape) -> Result<(), String>;

    /// Open a device with buffers sized for `shape`.
    fn open(&self, shape: Shape) -> Result<Self::Device, String>;

    /// True when a device opened at one shape can only ever launch THAT shape,
    /// so measuring a different one means opening a new device.
    ///
    /// False for OpenCL: `do_group_block_mining_opencl` takes work_groups,
    /// local_size and unit_size per launch, so one allocation sized at the top
    /// of a grid serves every point under it. True for CUDA: `cuda_mine_batch`
    /// reads `unit_size` from the miner it was built with and hands THAT to the
    /// kernel, so a CudaMiner allocated at unit_size 64 cannot be asked for 128
    /// at all - it would silently run 64 again and report the wrong shape's
    /// number.
    ///
    /// The auto-tuner asks this and nothing else about the difference. Getting
    /// it wrong in the safe direction costs a device open per candidate; getting
    /// it wrong the other way would have every CUDA candidate measure the same
    /// unit_size and the tuner would then write a shape it never ran.
    fn device_is_bound_to_its_shape(&self) -> bool;
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

// ---------------------------------------------------------------------------
// Backend: OpenCL
// ---------------------------------------------------------------------------

#[cfg(feature = "ocl")]
use crate::opencl_gpu::{OpenCLResources, block::do_group_block_mining_opencl_shares};

/// The OpenCL device under test, opened at one shape.
#[cfg(feature = "ocl")]
pub struct OclGateDevice {
    resources: OpenCLResources,
    shape: Shape,
}

#[cfg(feature = "ocl")]
impl OclGateDevice {
    /// A launch this device's buffers can actually hold. See
    /// [`launch_fits_allocation`], which is where the rule lives so it can be
    /// tested without a card.
    fn check_launch(&self, shape: Shape) -> Result<(), String> {
        launch_fits_allocation(self.shape, shape)
    }
}

#[cfg(feature = "ocl")]
impl GateDevice for OclGateDevice {
    fn allocated_shape(&self) -> Shape {
        self.shape
    }

    fn can_launch(&self, shape: Shape) -> bool {
        self.check_launch(shape).is_ok()
    }

    fn count_and_shares(
        &self,
        shape: Shape,
        height: u64,
        intro: &[u8],
        nonce_start: u32,
        share_target: &[u8; 32],
    ) -> Result<(u64, Vec<(u32, [u8; 32])>), String> {
        self.check_launch(shape)?;
        let out = do_group_block_mining_opencl_shares(
            &self.resources,
            height,
            intro.to_vec(),
            nonce_start,
            shape.work_groups,
            shape.local_size,
            shape.unit_size,
            Some(share_target),
        )
        .map_err(|e| e.display())?;
        Ok((out.share_hits, out.shares))
    }

    fn best(
        &self,
        shape: Shape,
        height: u64,
        intro: &[u8],
        nonce_start: u32,
    ) -> Result<(u32, [u8; 32]), String> {
        self.check_launch(shape)?;
        crate::opencl_gpu::block::do_group_block_mining_opencl(
            &self.resources,
            height,
            intro.to_vec(),
            nonce_start,
            shape.work_groups,
            shape.local_size,
            shape.unit_size,
        )
        .map_err(|e| e.display())
    }
}

/// Which OpenCL device to open, and from which kernel tree.
///
/// The kernel directory is part of the backend because OpenCL compiles at
/// RUNTIME, which is what lets `scripts/x16rs_gate_trees.py` prove the gate can
/// fail: point it at a tree with a deliberate defect and the same binary catches
/// it. CUDA has no equivalent knob at this level; see [`CudaBackend`].
#[cfg(feature = "ocl")]
pub struct OclBackend {
    pub opencl_dir: String,
    pub platform: u32,
    pub device: String,
}

#[cfg(feature = "ocl")]
impl GateBackend for OclBackend {
    type Device = OclGateDevice;

    fn name(&self) -> &'static str {
        "opencl"
    }

    fn describe(&self) -> String {
        format!(
            "platform {}, device_ids '{}', kernels from {}",
            self.platform, self.device, self.opencl_dir
        )
    }

    fn share_capacity(&self) -> u64 {
        crate::opencl_gpu::SHARE_LIST_CAPACITY as u64
    }

    fn check_shape(&self, shape: Shape) -> Result<(), String> {
        if shape.work_groups == 0 || shape.local_size == 0 || shape.unit_size == 0 {
            return Err(format!("launch shape {shape} has a zero dimension"));
        }
        Ok(())
    }

    fn open(&self, shape: Shape) -> Result<Self::Device, String> {
        Ok(OclGateDevice {
            resources: open_device(&self.opencl_dir, self.platform, &self.device, shape)?,
            shape,
        })
    }

    fn device_is_bound_to_its_shape(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Backend: CUDA
// ---------------------------------------------------------------------------

/// The CUDA device under test, opened at one shape.
///
/// `x16rs_cuda::CudaMiner` owns its device allocations and sizes them from
/// (work_groups, 256, unit_size) at construction, exactly like the OpenCL
/// resources, so the gate opens one per shape and drops it after.
#[cfg(feature = "cuda")]
pub struct CudaGateDevice {
    miner: x16rs_cuda::CudaMiner,
    shape: Shape,
}

#[cfg(feature = "cuda")]
impl CudaGateDevice {
    /// A launch this miner can really perform. See
    /// [`launch_fits_bound_allocation`], which is where the rule lives so it
    /// can be tested without a card.
    fn check_launch(&self, shape: Shape) -> Result<(), String> {
        launch_fits_bound_allocation(self.shape, shape)
    }
}

#[cfg(feature = "cuda")]
impl GateDevice for CudaGateDevice {
    fn allocated_shape(&self) -> Shape {
        self.shape
    }

    fn can_launch(&self, shape: Shape) -> bool {
        self.check_launch(shape).is_ok()
    }

    fn count_and_shares(
        &self,
        shape: Shape,
        height: u64,
        intro: &[u8],
        nonce_start: u32,
        share_target: &[u8; 32],
    ) -> Result<(u64, Vec<(u32, [u8; 32])>), String> {
        self.check_launch(shape)?;
        let out = self
            .miner
            .mine_block_batch_shares(
                height,
                intro,
                nonce_start,
                shape.work_groups,
                Some(share_target),
            )
            .map_err(|e| e.to_string())?;
        Ok((out.share_hits, out.shares))
    }

    fn best(
        &self,
        shape: Shape,
        height: u64,
        intro: &[u8],
        nonce_start: u32,
    ) -> Result<(u32, [u8; 32]), String> {
        self.check_launch(shape)?;
        self.miner
            .mine_block_batch(height, intro, nonce_start, shape.work_groups)
            .map_err(|e| e.to_string())
    }
}

/// True when this binary actually contains compiled CUDA kernels.
///
/// The `cuda` feature only adds the x16rs-cuda crate. Whether that crate holds
/// kernels is decided by its build script finding nvcc, which sets
/// `cfg(cuda_available)`; without it the crate still compiles and every device
/// call returns `NotCompiled`. Callers must check this before claiming a CUDA
/// run proved anything, and the gate binary exits non-zero when it is false.
#[cfg(feature = "cuda")]
pub fn cuda_kernels_available() -> bool {
    x16rs_cuda::CudaMiner::is_available()
}

/// Which CUDA device to open.
///
/// There is no kernel-directory knob here and there cannot be one at runtime:
/// `x16rs-cuda/build.rs` compiles `block_miner.cu` with nvcc into the static
/// library at BUILD time, so a CUDA kernel change means a rebuild. Fault
/// injection is still available, and against the very same defects the OpenCL
/// gate was proved with, because `block_miner.cu` includes `util.cl`,
/// `sha3_256.cl` and `x16rs.cl` straight out of `x16rs/opencl`: set
/// `X16RS_CUDA_KERNEL_DIR` to a tree built by `scripts/x16rs_gate_trees.py` and
/// rebuild, and this gate is running the broken algorithm on the card.
#[cfg(feature = "cuda")]
pub struct CudaBackend {
    pub device_index: i32,
}

#[cfg(feature = "cuda")]
impl GateBackend for CudaBackend {
    type Device = CudaGateDevice;

    fn name(&self) -> &'static str {
        "cuda"
    }

    fn describe(&self) -> String {
        match x16rs_cuda::CudaMiner::list_devices() {
            Ok(devices) => match devices.iter().find(|d| d.index == self.device_index) {
                Some(d) => format!(
                    "device #{} {} (SM {}.{}, {} MPs)",
                    d.index, d.name, d.compute_major, d.compute_minor, d.multiprocessor_count
                ),
                None => format!(
                    "device #{} (not present; {} device(s) visible)",
                    self.device_index,
                    devices.len()
                ),
            },
            Err(e) => format!("device #{} (enumeration failed: {e})", self.device_index),
        }
    }

    fn share_capacity(&self) -> u64 {
        x16rs_cuda::SHARE_LIST_CAPACITY as u64
    }

    fn check_shape(&self, shape: Shape) -> Result<(), String> {
        if shape.work_groups == 0 || shape.local_size == 0 || shape.unit_size == 0 {
            return Err(format!("launch shape {shape} has a zero dimension"));
        }
        // Not a limitation of the gate. `x16rs_cuda_main` declares
        // `__shared__ unsigned int local_nonces[256]` and reduces over a
        // power-of-two tree across the block, so any other block size either
        // overruns that array or reduces the wrong pairs. The host fixes it at
        // DEFAULT_LOCAL_SIZE for the same reason and refuses a device whose
        // maxThreadsPerBlock is lower. Say so here rather than let the operator
        // read a wrong-hash report caused by their own --local-size.
        if shape.local_size != x16rs_cuda::DEFAULT_LOCAL_SIZE {
            return Err(format!(
                "CUDA runs at local_size = {} only ({shape} was asked for): block_miner.cu's \
                 shared local_nonces[{}] and its power-of-two tree reduction are correct at that \
                 block size and no other",
                x16rs_cuda::DEFAULT_LOCAL_SIZE,
                x16rs_cuda::DEFAULT_LOCAL_SIZE
            ));
        }
        Ok(())
    }

    fn open(&self, shape: Shape) -> Result<Self::Device, String> {
        self.check_shape(shape)?;
        let miner =
            x16rs_cuda::CudaMiner::new(self.device_index, shape.work_groups, shape.unit_size)
                .map_err(|e| {
                    format!(
                        "opening CUDA device #{} at {shape}: {e}",
                        self.device_index
                    )
                })?;
        Ok(CudaGateDevice { miner, shape })
    }

    fn device_is_bound_to_its_shape(&self) -> bool {
        true
    }
}

/// Everything `equiv` needs that is not the device.
#[derive(Clone, Copy, Debug)]
pub struct EquivParams {
    /// Corpus headers to hash.
    pub headers: u32,
    /// Exhaustive windows per (shape, height, header).
    pub batches: u32,
    /// The shape the rig actually mines with, or a zero `prod_batches` to skip.
    pub prod_shape: Shape,
    pub prod_batches: u32,
    /// Rank thresholds used by the production count check.
    pub prod_thresholds: u32,
    /// CPU threads for the oracle.
    pub threads: usize,
}

/// Full byte-equivalence run, on whichever backend is handed in.
///
/// `headers` corpus entries x `batches` exhaustive windows x every shape in
/// `EXHAUSTIVE_SHAPES` x every repeat in `GATE_HEIGHTS`, plus `prod_batches`
/// runs at the production shape.
///
/// This function is the gate. Both backends run this exact code over this exact
/// corpus against this exact oracle, so a defect either card has is judged by
/// one standard, and a change to the standard cannot reach one backend and miss
/// the other.
#[cfg(any(feature = "ocl", feature = "cuda"))]
pub fn run_equivalence_on<B: GateBackend>(
    backend: &B,
    params: EquivParams,
) -> Result<EquivReport, String> {
    let EquivParams {
        headers,
        batches,
        prod_shape,
        prod_batches,
        prod_thresholds,
        threads,
    } = params;
    let started = Instant::now();
    let mut report = EquivReport {
        backend: backend.name().to_string(),
        device: backend.describe(),
        asked_exhaustive: headers > 0 && batches > 0,
        asked_production: prod_batches > 0,
        ..Default::default()
    };

    // The exhaustive mode is only exhaustive if the share list really is as big
    // as this module thinks. A backend whose capacity moved would silently turn
    // every "ENTIRE window" claim into a sample, so refuse before hashing
    // anything.
    let capacity = backend.share_capacity();
    if capacity != SHARE_LIST_CAPACITY {
        return Err(format!(
            "{} reports a share list capacity of {capacity}, the gate is built for \
             {SHARE_LIST_CAPACITY}; the exhaustive shapes would no longer dump whole windows",
            backend.name()
        ));
    }

    // Exhaustive pass. One device open per shape, because the GPU buffers are
    // sized from unit_size at init.
    for (work_groups, local_size, unit_size) in EXHAUSTIVE_SHAPES {
        let shape = Shape {
            work_groups,
            local_size,
            unit_size,
        };
        if shape.nonces() != capacity {
            return Err(format!(
                "exhaustive shape {shape} hashes {} nonces but the share list holds {capacity}; \
                 an exhaustive dump needs them equal",
                shape.nonces()
            ));
        }
        backend.check_shape(shape)?;
        let device = backend.open(shape)?;
        let count = shape.nonces() as u32;
        for height in GATE_HEIGHTS {
            let repeat = x16rs::block_hash_repeat(height);
            for header_index in 0..headers {
                let intro = corpus_header(header_index);
                for batch in 0..batches {
                    // A distinct nonce base per (shape, height, header, batch)
                    // so the gate never re-tests the same hashes twice.
                    let nonce_start = nonce_base(unit_size, height, header_index, batch);
                    let gpu = device.dump_window(shape, height, &intro, nonce_start)?;
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
        drop(device);
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
        backend.check_shape(prod_shape)?;
        let device = backend.open(prod_shape)?;
        let height = REPEAT16_HEIGHT;
        let repeat = x16rs::block_hash_repeat(height);
        let window = prod_shape.nonces();
        if window > u32::MAX as u64 {
            return Err("production window exceeds the 32-bit nonce space".to_string());
        }
        let ranks = threshold_ranks(window, capacity, prod_thresholds);
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
                let (share_hits, shares) =
                    device.count_and_shares(prod_shape, height, &intro, nonce_start, &target)?;
                report.production_count_checks += 1;
                if share_hits != rank {
                    return Err(format!(
                        "{DETECTED}production shape, header {header_index}, nonce base {nonce_start}: \
                         the CPU says exactly {rank} of the {window} hashes are <= {}, the kernel counted {share_hits}. \
                         The kernel's hashes differ from the CPU's somewhere in the window.",
                        hex::encode(target),
                    ));
                }
                // Whatever did come back must be byte-exact and in-window.
                for (nonce, hash) in &shares {
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
            let (best_nonce, best_hash) = device.best(prod_shape, height, &intro, nonce_start)?;
            if best_hash != sorted[0] {
                return Err(format!(
                    "{DETECTED}production shape best-hash reduction returned {} for nonce {best_nonce}; \
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
        drop(device);
    }

    report.wall_seconds = started.elapsed().as_secs_f64();
    Ok(report)
}

/// Byte-equivalence run on an OpenCL device.
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
    run_equivalence_on(
        &OclBackend {
            opencl_dir: opencl_dir.to_string(),
            platform,
            device: device.to_string(),
        },
        EquivParams {
            headers,
            batches,
            prod_shape,
            prod_batches,
            prod_thresholds,
            threads,
        },
    )
}

/// Byte-equivalence run on a CUDA device.
///
/// Same corpus, same oracle, same comparison, same report as the OpenCL entry
/// point above: the only difference between them is which three device calls
/// [`run_equivalence_on`] ends up making.
#[cfg(feature = "cuda")]
pub fn run_equivalence_cuda(
    device_index: i32,
    params: EquivParams,
) -> Result<EquivReport, String> {
    run_equivalence_on(&CudaBackend { device_index }, params)
}

/// Nonce base that keeps every (shape, height, header, batch) window disjoint.
#[cfg(any(feature = "ocl", feature = "cuda"))]
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

#[cfg(any(feature = "ocl", feature = "cuda"))]
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

/// How often a compared nonce's algorithm chain is derived.
///
/// Deriving it costs `repeat` extra CPU hashes, which at 1-in-1 would double the
/// oracle. 1-in-64 still gives hundreds of samples of every algorithm on a
/// default run, which is all the coverage count and the attribution's
/// passing-chain half need.
#[cfg(any(feature = "ocl", feature = "cuda"))]
const ALGO_SAMPLE_EVERY: u64 = 64;

#[cfg(any(feature = "ocl", feature = "cuda"))]
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
    if report.compared % ALGO_SAMPLE_EVERY == 0 {
        let chain = algo_sequence(height, intro, nonce);
        let mut ran = [false; 16];
        for algo in &chain {
            report.algo_counts[(*algo & 0x0f) as usize] += 1;
            ran[(*algo & 0x0f) as usize] = true;
        }
        // The passing half of the blame test. Counted once per chain, and only
        // for nonces the card got RIGHT: an algorithm that appears here cannot
        // be the broken one, because running it produced a correct hash.
        if gpu == cpu {
            report.passing_chain_samples += 1;
            for (index, hit) in ran.iter().enumerate() {
                if *hit {
                    report.passing_algo_chains[index] += 1;
                }
            }
        }
    }
    if gpu != cpu {
        if report.mismatches.len() < MAX_STORED_MISMATCHES {
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
#[cfg(any(feature = "ocl", feature = "cuda"))]
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
#[cfg(any(feature = "ocl", feature = "cuda"))]
#[derive(Clone, Debug)]
pub struct BaselineRun {
    pub seconds: f64,
    pub nonces: u64,
    pub hashrate: f64,
}

#[cfg(any(feature = "ocl", feature = "cuda"))]
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

#[cfg(any(feature = "ocl", feature = "cuda"))]
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
             disagreed by ~{:.1}% on this rig. To compare two OPENCL kernel trees, use\n                     \
             `x16rs_gate ab`, which alternates them inside one process and resolves ~0.3%.\n                     \
             CUDA kernels are compiled into the binary by nvcc, so no in-process A/B\n                     \
             exists for them: two CUDA builds can only be compared across processes, and\n                     \
             a difference under the ~{:.1}% between-process spread is not a result.\n",
            crate::bench_mainnet_repeat16::fmt_rate(self.median()),
            crate::bench_mainnet_repeat16::fmt_rate(self.min()),
            crate::bench_mainnet_repeat16::fmt_rate(self.max()),
            spread_p10_p90(&self.sorted_rates(), self.median()),
            self.spread_pct(),
            BETWEEN_PROCESS_SPREAD_PCT,
            BETWEEN_PROCESS_SPREAD_PCT,
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
#[cfg(any(feature = "ocl", feature = "cuda"))]
pub fn run_baseline_on<B: GateBackend>(
    backend: &B,
    shape: Shape,
    height: u64,
    batches_per_run: u32,
    runs: u32,
    warmup_batches: u32,
    headers: u32,
) -> Result<BaselineReport, String> {
    backend.check_shape(shape)?;
    let device = backend.open(shape)?;
    let per_batch = shape.nonces();
    let nonce_start = 0x1000_0000u32;
    let headers = headers.max(1);
    let header_indices: Vec<u32> = (0..batches_per_run).map(|b| b % headers).collect();
    let intros: Vec<Vec<u8>> = (0..headers).map(corpus_header).collect();

    // Warm-up: clocks, JIT, caches. Outside the timed region and outside the
    // corpus, so the measured work is unchanged by how long the warm-up ran.
    for w in 0..warmup_batches {
        let start = 0xF000_0000u32.wrapping_add(w.wrapping_mul(per_batch as u32));
        device
            .best(shape, height, &intros[0], start)
            .map_err(|e| format!("warm-up batch {}: {e}", w + 1))?;
    }

    let mut out_runs = Vec::with_capacity(runs as usize);
    let mut cpu_spot_checks = 0u32;
    for run in 0..runs {
        let started = Instant::now();
        let mut results: Vec<(u32, [u8; 32], u32)> = Vec::with_capacity(batches_per_run as usize);
        for batch in 0..batches_per_run {
            let header_index = header_indices[batch as usize];
            let start = nonce_start.wrapping_add(batch.wrapping_mul(per_batch as u32));
            let (nonce, hash) = device
                .best(shape, height, &intros[header_index as usize], start)
                .map_err(|e| format!("run {} batch {}: {e}", run + 1, batch + 1))?;
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

/// Fixed-work baseline on an OpenCL device.
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
    run_baseline_on(
        &OclBackend {
            opencl_dir: opencl_dir.to_string(),
            platform,
            device: device.to_string(),
        },
        shape,
        height,
        batches_per_run,
        runs,
        warmup_batches,
        headers,
    )
}

/// Fixed-work baseline on a CUDA device.
#[cfg(feature = "cuda")]
pub fn run_baseline_cuda(
    device_index: i32,
    shape: Shape,
    height: u64,
    batches_per_run: u32,
    runs: u32,
    warmup_batches: u32,
    headers: u32,
) -> Result<BaselineReport, String> {
    run_baseline_on(
        &CudaBackend { device_index },
        shape,
        height,
        batches_per_run,
        runs,
        warmup_batches,
        headers,
    )
}

// ---------------------------------------------------------------------------
// Paired A/B
//
// OpenCL only, and not by omission. `ab` alternates two KERNEL TREES inside one
// process, which works because OpenCL compiles its kernels at runtime from a
// directory the caller names. `x16rs-cuda/build.rs` compiles block_miner.cu with
// nvcc into the static library, so a CUDA process contains exactly one kernel
// build and cannot alternate. Comparing two CUDA kernels means two binaries, and
// the paired-within-one-process trick that resolves 0.3% on this rig is simply
// not available there; `baseline`'s ~2.6% between-process figure is.
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

    /// Every exhaustive shape must dump a whole window on BOTH backends.
    ///
    /// `run_equivalence_on` refuses at runtime if a backend's capacity has moved,
    /// but that check only fires on a machine with a card. This one fires on any
    /// machine, and it is the thing that keeps the CUDA gate as exhaustive as the
    /// OpenCL one: if someone changes the share list to 512, the shapes stop
    /// covering the window and the "ENTIRE window" line in the report becomes a
    /// lie on both cards at once.
    #[test]
    fn every_exhaustive_shape_fills_the_share_list_exactly() {
        for (work_groups, local_size, unit_size) in EXHAUSTIVE_SHAPES {
            let shape = Shape {
                work_groups,
                local_size,
                unit_size,
            };
            assert_eq!(
                shape.nonces(),
                SHARE_LIST_CAPACITY,
                "{shape} does not dump exactly one full share list"
            );
            // CUDA fixes the block size at 256; a shape that broke that would be
            // rejected at run time on NVIDIA and silently accepted on AMD, which
            // is exactly the drift one gate with two backends is meant to stop.
            assert_eq!(shape.local_size, 256, "{shape} is not launchable on CUDA");
        }
        // Three DIFFERENT unit_sizes, because unit_size is what varies the
        // counting sort's ordering and the histogram contention inside a work
        // group. Three copies of one shape would prove a third as much.
        let mut units: Vec<u32> = EXHAUSTIVE_SHAPES.iter().map(|s| s.2).collect();
        units.sort_unstable();
        units.dedup();
        assert_eq!(units.len(), EXHAUSTIVE_SHAPES.len());
    }

    /// A helper that builds a report as if the card had failed exactly the
    /// nonces whose chain runs `broken`, which is what one broken algorithm
    /// function does.
    fn report_with_broken_algo(broken: u8, height: u64, nonces: u32) -> EquivReport {
        let intro = corpus_header(2);
        let mut report = EquivReport {
            backend: "test".into(),
            ..Default::default()
        };
        for nonce in 0..nonces {
            let chain = algo_sequence(height, &intro, nonce);
            let hit = chain.contains(&broken);
            if hit {
                report.mismatches.push(Mismatch {
                    height,
                    repeat: x16rs::block_hash_repeat(height),
                    header_index: 2,
                    nonce,
                    gpu: [0u8; 32],
                    cpu: [1u8; 32],
                    algos: chain,
                });
            } else {
                report.passing_chain_samples += 1;
                let mut seen = [false; 16];
                for algo in &chain {
                    seen[(*algo & 0x0f) as usize] = true;
                }
                for (index, ran) in seen.iter().enumerate() {
                    if *ran {
                        report.passing_algo_chains[index] += 1;
                    }
                }
            }
        }
        report
    }

    /// The whole point of the attribution: one broken algorithm, named, at every
    /// repeat the gate runs.
    ///
    /// Repeat 16 is the hard case. An innocent algorithm is in a random chain
    /// with probability 1 - (15/16)^16 = 64%, so the "in every failure"
    /// intersection alone leaves several suspects; only the "in no passing
    /// chain" half removes them. If someone deletes that half, this test fails
    /// at 16 and passes at 1, which is the diagnosis printed in the assertion.
    #[test]
    fn one_broken_algorithm_is_named_at_every_repeat() {
        for height in GATE_HEIGHTS {
            for broken in 0u8..16 {
                let report = report_with_broken_algo(broken, height, 4_000);
                let blame = report.attribute();
                assert!(
                    blame.failing_chains >= MIN_CHAINS_TO_NAME,
                    "height {height} algo {broken}: only {} failing chains, the corpus is too \
                     small to test the attribution",
                    blame.failing_chains
                );
                assert_eq!(
                    blame.named,
                    vec![broken],
                    "height {height} (repeat {}): broken algorithm {} ({}) was not named alone; \
                     in every failure = {:?}, named = {:?}",
                    x16rs::block_hash_repeat(height),
                    broken,
                    ALGO_NAMES[broken as usize],
                    blame.in_every_failure,
                    blame.named,
                );
                assert_eq!(
                    blame.passing[broken as usize], 0,
                    "a chain that ran the broken algorithm cannot have passed"
                );
                assert!(blame.render().contains(ALGO_NAMES[broken as usize]));
            }
        }
    }

    /// A defect that is not one algorithm must NOT be blamed on one.
    ///
    /// The barrier fault the OpenCL gate was proved with corrupts hashes without
    /// regard to which algorithms they ran. Failing chains then share no
    /// algorithm, and the honest report is "no single algorithm is implicated" -
    /// which is also the true statement about a race.
    #[test]
    fn a_defect_that_is_not_one_algorithm_names_nothing() {
        let intro = corpus_header(4);
        let height = REPEAT16_HEIGHT;
        let mut report = EquivReport::default();
        // Every third nonce wrong, chosen with no reference to its chain.
        for nonce in 0..900u32 {
            let chain = algo_sequence(height, &intro, nonce);
            if nonce % 3 == 0 {
                report.mismatches.push(Mismatch {
                    height,
                    repeat: 16,
                    header_index: 4,
                    nonce,
                    gpu: [0u8; 32],
                    cpu: [1u8; 32],
                    algos: chain,
                });
            } else {
                report.passing_chain_samples += 1;
                let mut seen = [false; 16];
                for algo in &chain {
                    seen[(*algo & 0x0f) as usize] = true;
                }
                for (index, ran) in seen.iter().enumerate() {
                    if *ran {
                        report.passing_algo_chains[index] += 1;
                    }
                }
            }
        }
        let blame = report.attribute();
        assert!(
            blame.named.is_empty(),
            "a race was blamed on {:?}",
            blame.named
        );
        assert!(blame.render().contains("NO single algorithm"));
    }

    /// The passing-chain half of the attribution, pinned where it does the work.
    ///
    /// On a thin run the intersection of the failing chains is NOT one algorithm
    /// - here it is six - and only "ran in no chain the card got right" cuts it
    /// to the culprit. Delete that half and this test reports the six.
    #[test]
    fn the_passing_chain_filter_is_what_cuts_the_suspects_down() {
        // 6 corpus nonces at repeat 16, skein (5) broken: three chains fail.
        let report = report_with_broken_algo(5, REPEAT16_HEIGHT, 6);
        let blame = report.attribute();
        assert_eq!(blame.failing_chains, 3);
        assert!(
            blame.in_every_failure.len() > 1,
            "the intersection alone was already unique, so this test proves nothing: {:?}",
            blame.in_every_failure
        );
        assert!(blame.in_every_failure.contains(&5));
        assert_eq!(
            blame.named,
            vec![5],
            "the passing-chain filter should have cut {:?} to skein alone",
            blame.in_every_failure
        );
    }

    /// Too few failures must produce a candidate list, not a confident name.
    #[test]
    fn a_handful_of_mismatches_names_nothing() {
        let report = report_with_broken_algo(9, REPEAT16_HEIGHT, 4);
        let blame = report.attribute();
        assert!(blame.failing_chains < MIN_CHAINS_TO_NAME);
        assert!(blame.render().contains("too few failing chains"));
    }

    /// A clean run must not accuse anyone, and must print nothing about blame.
    #[test]
    fn a_passing_run_attributes_nothing() {
        let mut report = EquivReport {
            compared: 10_000,
            passing_chain_samples: 150,
            ..Default::default()
        };
        report.algo_counts = [40; 16];
        report.passing_algo_chains = [90; 16];
        assert!(report.passed());
        let blame = report.attribute();
        assert_eq!(blame.failing_chains, 0);
        assert!(blame.named.is_empty());
        assert_eq!(blame.render(), "");
    }

    /// A gate that compared nothing, or that never reached an algorithm, is not
    /// a pass. This is the property that makes a CUDA run on a machine with no
    /// device fail instead of printing an empty PASS.
    #[test]
    fn an_empty_or_incomplete_run_is_not_a_pass() {
        assert!(!EquivReport::default().passed());
        let mut compared_nothing = EquivReport {
            compared: 0,
            ..Default::default()
        };
        compared_nothing.algo_counts = [1; 16];
        assert!(!compared_nothing.passed());
        let mut missed_one = EquivReport {
            compared: 5_000,
            ..Default::default()
        };
        missed_one.algo_counts = [7; 16];
        missed_one.algo_counts[11] = 0;
        assert!(!missed_one.passed());
        assert!(missed_one.render().contains("NEVER TESTED"));
    }
}


// ---------------------------------------------------------------------------
// The launch-fit rules, which are what stop a measurement being attributed to a
// shape that never ran. Compiled and tested without a card, on purpose: neither
// rule needs one, and both are the kind of thing that is only ever exercised on
// hardware nobody has.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod launch_fit_tests {
    use super::*;

    fn shape(work_groups: u32, local_size: u32, unit_size: u32) -> Shape {
        Shape {
            work_groups,
            local_size,
            unit_size,
        }
    }

    #[test]
    fn a_per_call_backend_launches_anything_that_fits_its_buffers() {
        let allocated = shape(64, 256, 192);
        // The whole reason the auto-tuner opens one OpenCL device for a session:
        // every smaller point on the grid runs against the same allocation.
        assert!(launch_fits_allocation(allocated, allocated).is_ok());
        assert!(launch_fits_allocation(allocated, shape(32, 256, 64)).is_ok());
        assert!(launch_fits_allocation(allocated, shape(64, 256, 32)).is_ok());

        // Larger in either axis writes past global_hashes / global_order.
        assert!(launch_fits_allocation(allocated, shape(96, 256, 192)).is_err());
        assert!(launch_fits_allocation(allocated, shape(64, 256, 256)).is_err());
        // A different block size is a different indexing, not a smaller launch.
        assert!(launch_fits_allocation(allocated, shape(64, 128, 64)).is_err());
    }

    #[test]
    fn a_bound_backend_refuses_the_launch_that_would_lie_about_its_shape() {
        let allocated = shape(256, 256, 64);
        assert!(launch_fits_bound_allocation(allocated, allocated).is_ok());
        // Work groups ARE a per-launch parameter on this backend, so fewer of
        // them is a real launch of fewer and one miner serves the whole
        // work-group axis at its unit_size.
        assert!(launch_fits_bound_allocation(allocated, shape(128, 256, 64)).is_ok());
        assert!(launch_fits_bound_allocation(allocated, shape(48, 256, 64)).is_ok());

        // unit_size is not, and this is the case with no other line of defence:
        // the kernel would run 64, return hashes that are correct FOR 64, pass
        // every equivalence proof, and hand back a time the caller would file
        // under 128. Only the time would be wrong, so only refusing the launch
        // catches it.
        let mislabelled = launch_fits_bound_allocation(allocated, shape(256, 256, 128));
        let message = mislabelled.expect_err("a unit_size the miner was not built with");
        assert!(message.contains("would run unit_size 64"), "{message}");
        assert!(message.contains("reported as 128"), "{message}");

        assert!(launch_fits_bound_allocation(allocated, shape(512, 256, 64)).is_err());
        assert!(launch_fits_bound_allocation(allocated, shape(256, 128, 64)).is_err());
    }

    #[test]
    fn the_two_rules_disagree_exactly_where_the_backends_do() {
        // One shape, two backends, opposite answers, and that difference is the
        // entire reason `device_is_bound_to_its_shape` exists. A tuner that used
        // the permissive rule on the strict backend would measure the same
        // unit_size over and over and write a shape it never ran.
        let allocated = shape(256, 256, 128);
        let smaller_unit = shape(256, 256, 64);
        assert!(launch_fits_allocation(allocated, smaller_unit).is_ok());
        assert!(launch_fits_bound_allocation(allocated, smaller_unit).is_err());
    }
}
