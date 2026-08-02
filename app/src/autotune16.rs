//! Auto-tune, measured on the workload the miner really runs.
//!
//! The tuner this replaces measured at height 1, i.e. x16rs repeat = 1, while
//! every rig on the live chain runs repeat = 16. Our own comment admitted it
//! ("measures at height = 1 to keep tuning fast"), so every launch shape the
//! panel has ever chosen was optimised for a workload nobody mines. At repeat 1
//! the kernel runs one algorithm round per nonce instead of sixteen: the
//! per-hash cost profile, the register pressure and the memory behaviour are all
//! different, and so is the shape that wins.
//!
//! Four things are done differently here, and each one is a defect that was
//! costing the operator money rather than a refinement.
//!
//!   1. **repeat = 16, on a corpus that is frozen for the whole session.**
//!      Every candidate hashes exactly the same multiset of (header, nonce)
//!      pairs, in the same order. This matters more than it looks: x16rs picks
//!      each round's algorithm from the previous round's output, and the sixteen
//!      algorithms differ in cost by more than an order of magnitude, so a
//!      candidate handed a different nonce window can draw a cheaper algorithm
//!      mix and look faster while being slower. Same work, measure the time. Not
//!      same time, different work.
//!
//!   2. **A coarse sweep, then the finalists re-run in alternating order.**
//!      This card settles into a clock state for the life of a process and
//!      drifts by ~2.6% between processes; within one process, back-to-back
//!      measurements resolve ~0.3%. Running the finalists in the order A,B,C
//!      then C,B,A and taking each one's median cancels the drift that would
//!      otherwise hand the win to whichever candidate happened to run while the
//!      card was cool.
//!
//!   3. **A thermal soak on the winner, run until it stops moving.** The old
//!      final verification was 5 to 15 seconds, which is long enough to prove a
//!      shape runs and far too short to prove it sustains. This one hashes the
//!      corpus over and over while sampling temperature, board power, shader
//!      clock and hashrate, and only accepts the shape once all four have been
//!      flat across a window of passes. The time it took is reported, because on
//!      an air-cooled card it is minutes, not seconds.
//!
//!   4. **p95 batch latency is a constraint, and stale work is priced.** A batch
//!      is atomic: when the template changes, everything in flight is thrown
//!      away. At a 300-second target block time a batch of L seconds throws away
//!      L/2 seconds of work on average every time the job changes, so throughput
//!      is discounted by that fraction before anything is compared, and a
//!      candidate whose p95 batch exceeds the ceiling is refused outright no
//!      matter how fast it hashes.
//!
//! Scoring is on measured watts wherever the card reports them (see
//! `gpu_temp_adl`), so Eco really does optimise hashes per joule and Profit
//! really does optimise net income, instead of both optimising a number derived
//! from a board-power constant typed into an ini.
//!
//! Consensus safety. x16rs is consensus, so a shape whose hashes differ from the
//! CPU reference in one byte mines invalid blocks. Every candidate is proved
//! against the CPU oracle at its own launch shape before its speed is allowed to
//! count, and the proof covers its entire batch window rather than a sample: see
//! `prove_shape`. That is a *shape* proof. It does not replace `x16rs_gate
//! equiv`, which is the *kernel* proof, and the tuner prints that command
//! whenever a kernel is newer than the last time somebody ran it.
//!
//! # The corpus is sized by the clock, not by the biggest candidate
//!
//! The shared corpus is what makes two launch shapes comparable, and its segment
//! has to be a common multiple of every candidate's batch. The first version of
//! this module planned one segment over the whole candidate universe and then
//! forced at least four of them, so the smallest amount of work a candidate
//! could be measured on was 24 to 36 times the largest candidate's batch, and
//! the largest batch scales with the card's work-group ceiling. On the one card
//! this was written for that is 75 M nonces, about 2.6 s. On a preset allowed
//! 2048 or 4096 work groups it is 2.4 to 4.8 G nonces, and since a soak has to
//! fit five passes inside 90 s, finishing a tune needed 107 to 215 MH/s. The
//! only x16rs repeat-16 rate ever measured here is 28.8 MH/s. Every card except
//! the one it was written on swept for tens of minutes and then reported that
//! the card "never settled", which was not true: the pass was simply longer than
//! the window it had to settle in.
//!
//! Three things now bound that, and the order matters:
//!
//!   1. **A shape whose batch cannot meet the latency ceiling is not measured.**
//!      `score` already refuses any candidate whose p95 batch exceeds
//!      `P95_BATCH_CEILING_MS`, so a batch that takes longer than that was never
//!      going to be chosen. It was still being hashed, still being proved against
//!      the CPU oracle over its entire window, and still forcing everyone else's
//!      corpus segment up. The probe measures the card first, and shapes that
//!      cannot fit the ceiling at that rate are named and dropped before a single
//!      candidate is measured.
//!
//!   2. **The corpus segment has a cap in seconds, not in batches.** The cap is
//!      the shorter of the pass the sweep can afford (`budget` split over the
//!      passes it will make) and the pass the soak can settle on
//!      (`max_soak_pass_seconds`). Shapes that would push the segment past it are
//!      dropped, loudest first: the tuner prints each one and prints the
//!      `benchmark_seconds` that would have kept them. That is the honest trade
//!      and it is now the operator's to make: a short budget buys the 2x grid, a
//!      long budget buys the 1.5x grid.
//!
//!   3. **`segments` starts at one.** It is chosen from the probe so that a pass
//!      lands near the target, and it is never forced to four to satisfy a header
//!      count; the header count follows the segments instead.
//!
//! The property that made the corpus shared is untouched: every candidate still
//! covers the identical (header_index, nonce) multiset in the identical order,
//! and `coverage_signature` proves it element by element rather than asserting
//! it in a comment. What changed is only how big that multiset is.
//!
//! # Both backends, one tuner
//!
//! This measures through [`crate::x16rs_gate`]'s two traits, so a `--features
//! cuda` build tunes an NVIDIA card with the same corpus, the same CPU oracle,
//! the same latency ceiling, the same soak and the same equivalence proof an
//! OpenCL build uses on an AMD one. That is not a convenience. The whole reason
//! a tuner exists rather than a table is that two cards want opposite things:
//! on an RX 9070 XT the kernel is latency bound and unit_size 192 beats 64 by
//! about 9%, while on a Tesla T4 at repeat 16 the ordering REVERSES (64 -> 7.54
//! MH/s, 96 -> 7.19, 128 -> 7.06) because that card sits at 66 to 67 W against
//! a 70 W cap and a bigger batch cannot buy work the power limit will not
//! allow. Two cards, opposite optima, one standard for judging them.
//!
//! What the CUDA path cannot borrow from OpenCL is `x16rs_gate ab`, which
//! alternates two KERNEL TREES inside one process and resolves ~0.3%. nvcc
//! compiles CUDA kernels into the binary, so two CUDA kernels are two binaries.
//! Launch SHAPES are a different question and are alternated in-process here on
//! both backends; how finely that resolved is measured from the finalists' own
//! repeats and printed, rather than assumed. See `resolution_note`.

#[cfg(any(feature = "ocl", feature = "cuda", test))]
use crate::efficiency::{BenchmarkPick, EfficiencyMode};

// ---------------------------------------------------------------------------
// The fixed corpus
// ---------------------------------------------------------------------------

#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub use crate::x16rs_gate::Shape;

/// Greatest common divisor, iterative so a pathological pair cannot blow a
/// stack in a mining process.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// Least common multiple, or `None` on overflow.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
fn lcm(a: u64, b: u64) -> Option<u64> {
    if a == 0 || b == 0 {
        return None;
    }
    (a / gcd(a, b)).checked_mul(b)
}

/// The corpus every candidate in one tuning session processes.
///
/// It is cut into `segments` equal blocks of `segment_nonces` consecutive
/// nonces, and block number s is hashed against corpus header `s % headers`.
/// A candidate covers each segment with a whole number of its own batches,
/// which is what makes the (header, nonce) multiset identical for every
/// candidate rather than merely similar.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Corpus {
    pub nonce_start: u32,
    pub headers: u32,
    pub segment_nonces: u64,
    pub segments: u32,
}

/// One launch: which corpus header, and where in the nonce space.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CorpusBatch {
    pub header_index: u32,
    pub nonce_start: u32,
    pub nonces: u64,
}

#[cfg(any(feature = "ocl", feature = "cuda", test))]
impl Corpus {
    pub fn total_nonces(&self) -> u64 {
        self.segment_nonces.saturating_mul(self.segments as u64)
    }

    /// True when `shape` can tile every segment exactly.
    pub fn fits(&self, shape: Shape) -> bool {
        let batch = shape.nonces();
        batch > 0 && self.segment_nonces % batch == 0
    }

    /// The launches `shape` must perform to cover the corpus, in order.
    ///
    /// Refuses rather than truncates when the shape does not tile a segment: a
    /// candidate that covered 31/32 of every segment would be measured on less
    /// work than its rivals and would win by not doing it.
    pub fn batches(&self, shape: Shape) -> Result<Vec<CorpusBatch>, String> {
        let batch = shape.nonces();
        if !self.fits(shape) {
            return Err(format!(
                "launch shape {}x{}x{} hashes {batch} nonces, which does not divide the \
                 {}-nonce corpus segment; it cannot hash the same work as the other candidates",
                shape.work_groups, shape.local_size, shape.unit_size, self.segment_nonces
            ));
        }
        let total = self.total_nonces();
        if total == 0 || total > u32::MAX as u64 {
            return Err(format!(
                "corpus of {total} nonces does not fit the 32-bit nonce space"
            ));
        }
        if self.nonce_start as u64 + total > u32::MAX as u64 {
            return Err(format!(
                "corpus [{}, {}) runs off the end of the 32-bit nonce space",
                self.nonce_start,
                self.nonce_start as u64 + total
            ));
        }
        let per_segment = self.segment_nonces / batch;
        let mut out = Vec::with_capacity((per_segment * self.segments as u64) as usize);
        for segment in 0..self.segments as u64 {
            let header_index = (segment % self.headers.max(1) as u64) as u32;
            let base = self.nonce_start as u64 + segment * self.segment_nonces;
            for index in 0..per_segment {
                out.push(CorpusBatch {
                    header_index,
                    nonce_start: (base + index * batch) as u32,
                    nonces: batch,
                });
            }
        }
        Ok(out)
    }

    /// What `shape` will hash, reduced to the smallest form that still names
    /// every (header_index, nonce) pair: the maximal runs of consecutive nonces
    /// that carry one header, in the order they are hashed.
    ///
    /// Two shapes with equal signatures hash the identical sequence of
    /// (header_index, nonce) pairs. That is not an assertion about the code, it
    /// is a consequence of what this function checks on the way: every batch
    /// starts exactly where the previous one ended, so a coverage has no gap, no
    /// overlap and no reordering, and a gapless ordered cover is determined by
    /// its runs. `identical_coverage_is_exactly_an_identical_signature` proves
    /// the two forms agree by expanding both into individual pairs.
    ///
    /// This exists because the direct comparison does not fit in memory: one
    /// corpus is tens of millions of nonces and there are up to forty-five
    /// candidates, so materialising the pairs is gigabytes per shape.
    pub fn coverage_signature(&self, shape: Shape) -> Result<Vec<(u32, u32, u64)>, String> {
        let batches = self.batches(shape)?;
        let mut runs: Vec<(u32, u32, u64)> = Vec::new();
        let mut next_nonce: Option<u64> = None;
        for batch in &batches {
            let start = batch.nonce_start as u64;
            if let Some(expected) = next_nonce {
                if start != expected {
                    return Err(format!(
                        "launch shape {}x{}x{} would hash nonce {start} straight after nonce {}; \
                         its coverage is not a gapless ordered cover of the corpus",
                        shape.work_groups,
                        shape.local_size,
                        shape.unit_size,
                        expected - 1
                    ));
                }
            }
            next_nonce = Some(start + batch.nonces);
            match runs.last_mut() {
                Some(run) if run.0 == batch.header_index && run.1 as u64 + run.2 == start => {
                    run.2 += batch.nonces;
                }
                _ => runs.push((batch.header_index, batch.nonce_start, batch.nonces)),
            }
        }
        let covered: u64 = runs.iter().map(|run| run.2).sum();
        if covered != self.total_nonces() {
            return Err(format!(
                "launch shape {}x{}x{} covers {covered} of the corpus's {} nonces",
                shape.work_groups,
                shape.local_size,
                shape.unit_size,
                self.total_nonces()
            ));
        }
        Ok(runs)
    }
}

/// The smallest segment size every one of `batch_sizes` divides exactly, scaled
/// up to at least `min_nonces`.
///
/// `None` when the answer would exceed `cap`. The caller's job then is to drop
/// the candidate that is forcing the quantum up and try again, which is honest:
/// a shape that cannot share the corpus cannot be compared on it.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn shared_segment_nonces(batch_sizes: &[u64], min_nonces: u64, cap: u64) -> Option<u64> {
    let mut quantum = 1u64;
    for size in batch_sizes {
        quantum = lcm(quantum, *size)?;
        if quantum > cap {
            return None;
        }
    }
    let multiple = min_nonces.div_ceil(quantum.max(1)).max(1);
    let scaled = quantum.checked_mul(multiple)?;
    (scaled <= cap).then_some(scaled)
}

/// The corpus segment a set of shapes needs, given that a segment must be at
/// least `min_nonces` and no more than `cap`.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
fn segment_for(shapes: &[Shape], min_nonces: u64, cap: u64) -> Option<u64> {
    let sizes: Vec<u64> = shapes.iter().map(|shape| shape.nonces()).collect();
    shared_segment_nonces(&sizes, min_nonces, cap)
}

/// Build a corpus that every shape in `shapes` can tile, dropping the shapes
/// that force the quantum past `cap`.
///
/// Returns the corpus, the shapes that survived and the shapes that were
/// dropped, so the caller can say which candidates it is not going to measure
/// and why, instead of quietly measuring them on different work.
///
/// `cap` is a wall-clock decision, not a memory one: `plan_session` sets it to
/// the nonces this card hashes in the longest pass the sweep and the soak can
/// both afford. A shape dropped here is a shape the operator's budget cannot
/// pay for, and the tuner says so along with the budget that would.
///
/// The shape dropped at each step is the one whose removal shrinks the quantum
/// the most, which is not the same as the one with the largest batch: a batch of
/// 216 832 nonces (7 x 256 x 121) forces the quantum up by a factor of 847 while
/// a batch of 262 144 (32 x 256 x 32) divides it away entirely. Dropping by size
/// would throw away the useful shape and keep the awkward one.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn plan_corpus(
    shapes: &[Shape],
    nonce_start: u32,
    headers: u32,
    segments: u32,
    min_segment_nonces: u64,
    cap_segment_nonces: u64,
) -> Result<(Corpus, Vec<Shape>, Vec<Shape>), String> {
    if shapes.is_empty() {
        return Err("no candidate launch shapes".to_string());
    }
    let mut kept: Vec<Shape> = shapes.to_vec();
    let mut dropped = Vec::new();
    loop {
        if let Some(segment_nonces) =
            segment_for(&kept, min_segment_nonces, cap_segment_nonces)
        {
            let corpus = Corpus {
                nonce_start,
                headers: headers.max(1),
                segment_nonces,
                segments: segments.max(1),
            };
            kept.sort_by_key(|shape| (shape.work_groups, shape.unit_size));
            return Ok((corpus, kept, dropped));
        }
        if kept.len() <= 1 {
            return Err(format!(
                "no corpus segment under {cap_segment_nonces} nonces can be tiled by the \
                 candidate launch shapes; the last one hashes {} nonces per batch",
                kept.first().map(|s| s.nonces()).unwrap_or(0)
            ));
        }
        // Which shape is inflating the quantum? A least common multiple is
        // (the largest power of two) x (the l.c.m. of the odd parts), and it is
        // the odd part that hurts: one batch of 216 832 = 2^8 x 847 multiplies
        // everyone else's segment by 847, while a batch of 262 144 = 2^18,
        // though larger, multiplies it by nothing. So the shape removed is the
        // one with the largest odd factor, and only where those tie does size
        // decide. Picking by size alone would delete the useful shape and keep
        // the awkward one.
        let odd_part = |mut value: u64| {
            while value % 2 == 0 && value > 0 {
                value /= 2;
            }
            value
        };
        let worst = (0..kept.len())
            .max_by_key(|index| {
                let batch = kept[*index].nonces();
                (odd_part(batch), batch)
            })
            .unwrap_or(0);
        dropped.push(kept.remove(worst));
    }
}

// ---------------------------------------------------------------------------
// Where the corpus is placed, and how big it is allowed to be
// ---------------------------------------------------------------------------

/// Where a tuning corpus starts in the 32-bit nonce space.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub const NONCE_BASE: u32 = 0x2000_0000;

/// Where the probe hashes, kept clear of the corpus so a probe batch can never
/// be mistaken for corpus work.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub const PROBE_NONCE_BASE: u32 = 0x1000_0000;

/// Smallest corpus segment worth timing. Below about a quarter of a second the
/// per-launch overheads and the sampler's 100 ms period start to show.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub const MIN_SEGMENT_NONCES: u64 = 1 << 21;

/// The share of the operator's budget the sweeps may spend. The soak runs on
/// top of the budget, so the sweeps are not allowed all of it.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub const SWEEP_BUDGET_SHARE: f64 = 0.8;

/// Passes the refinement stage is budgeted for: two finalists, up to four
/// neighbours each on a two-axis grid.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub const REFINE_PASS_ALLOWANCE: u32 = 8;

/// Passes the final round is budgeted for: three finalists, three passes each.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub const FINAL_PASS_ALLOWANCE: u32 = 9;

/// How much of the longest settleable pass the plan is allowed to use.
///
/// The soak's arithmetic gives a hard ceiling; a card that turns out slower than
/// its probe said, or a first pass that carries a kernel upload, must not push
/// the plan over it. 0.6 leaves the soak room for a pass two thirds longer than
/// planned and still settle.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub const SOAK_PASS_MARGIN: f64 = 0.6;

/// How much faster than the probe shape a large shape is allowed to be before
/// the latency prune stops believing the probe.
///
/// The probe runs the smallest candidate, and small shapes under-feed this
/// kernel: on the RX 9070 XT the shipped 48x256x48 measured 19.13 MH/s against
/// 28.80 for 64x256x192, a factor of 1.51. 1.6 keeps the prune on the generous
/// side of the only spread anyone has measured, so a shape is dropped for
/// latency only when it cannot fit the ceiling even at the best rate this
/// kernel has ever shown.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub const LATENCY_HEADROOM: f64 = 1.6;

/// Passes that must be flat together before a soak is accepted.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn soak_window_passes() -> usize {
    SettleLimits::default().window.max(2)
}

/// The soak's wall-clock cap. Long enough for an air-cooled card to reach a
/// steady temperature, and larger when the operator's budget is larger.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn soak_cap_seconds(budget_seconds: u64) -> f64 {
    (budget_seconds as f64 * 0.5).max(90.0).min(900.0)
}

/// The soak's minimum duration, so a shape cannot be declared settled on five
/// passes taken over fifteen seconds.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn soak_floor_seconds(budget_seconds: u64) -> f64 {
    45.0f64.min(soak_cap_seconds(budget_seconds))
}

/// The longest one corpus pass may take if the soak is to reach its settling
/// window at all.
///
/// `soak_until_settled` begins a pass only while `elapsed < cap`, so to begin
/// the `w`-th pass it must have spent less than `cap` on the first `w - 1`.
/// A pass of `p` seconds therefore reaches the window only when
/// `(w - 1) * p < cap`. This is the arithmetic the previous version of this
/// module did not check anywhere: a card whose pass exceeded it soaked for the
/// whole cap, never reached five passes, and was told to re-run with a larger
/// `benchmark_seconds`, which does not shorten a pass.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn max_soak_pass_seconds(budget_seconds: u64) -> f64 {
    soak_cap_seconds(budget_seconds) / (soak_window_passes() as f64 - 1.0)
}

/// Everything one tuning session will measure, and what it will cost, decided
/// before a single candidate is measured.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
#[derive(Clone, Debug)]
pub struct SessionPlan {
    pub corpus: Corpus,
    /// The coarse sweep, after both prunes, in the order it will be measured.
    pub candidates: Vec<Shape>,
    /// Every shape the corpus can tile, which is the pool refinement draws from.
    pub usable: Vec<Shape>,
    /// Dropped because their batch cannot meet the p95 latency ceiling at the
    /// probed rate, so they could never have been chosen.
    pub over_ceiling: Vec<Shape>,
    /// Dropped because they would push the corpus segment past what the budget
    /// can pay for.
    pub off_corpus: Vec<Shape>,
    pub probe_hps: f64,
    /// One pass of the corpus, at the probed rate.
    pub pass_seconds: f64,
    /// The cap that decided the corpus segment.
    pub pass_ceiling_seconds: f64,
    pub sweep_passes: u32,
    pub sweep_seconds: f64,
    /// The `benchmark_seconds` at which nothing would have been dropped for
    /// cost, or `None` when no budget can buy them back.
    pub budget_for_every_shape: Option<u64>,
}

#[cfg(any(feature = "ocl", feature = "cuda", test))]
impl SessionPlan {
    /// A tune is a comparison. One shape measured is a report.
    pub fn is_a_comparison(&self) -> bool {
        self.candidates.len() >= 2
    }

    /// True when the soak can reach its settling window on this corpus at the
    /// probed rate. `plan_session` refuses to return a plan where it is false.
    pub fn soak_can_settle(&self, budget_seconds: u64) -> bool {
        self.pass_seconds > 0.0 && self.pass_seconds < max_soak_pass_seconds(budget_seconds)
    }
}

/// Decide the whole session: which shapes are worth measuring, on what corpus,
/// and what that will cost in wall-clock seconds.
///
/// This is the function the fix lives in, and it is deliberately free of the
/// device so it can be driven from a test at any hashrate for any card in
/// `PANEL_GPU_PRESETS`. `tune` calls it and does what it says.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
#[allow(clippy::too_many_arguments)]
pub fn plan_session(
    min_work_groups: u32,
    max_work_groups: u32,
    max_unit_size: u32,
    local_size: u32,
    probe_hps: f64,
    budget_seconds: u64,
    headers: u32,
    nonce_start: u32,
) -> Result<SessionPlan, String> {
    if !probe_hps.is_finite() || probe_hps <= 0.0 {
        return Err(format!(
            "the probe measured {probe_hps} hashes per second, which is not a rate a corpus can \
             be sized from"
        ));
    }
    let universe = candidate_universe(
        min_work_groups,
        max_work_groups,
        max_unit_size,
        local_size,
    );
    let coarse = coarse_candidates(
        min_work_groups,
        max_work_groups,
        max_unit_size,
        local_size,
    );
    if universe.is_empty() || coarse.is_empty() {
        return Err("no launch shape fits this device's limits".to_string());
    }

    // 1. The latency prune. `score` refuses a candidate whose p95 batch is over
    //    the ceiling, so a shape whose single batch cannot fit it is work that
    //    buys nothing: it cannot win, and it drags the shared corpus, the CPU
    //    oracle in `prove_shape` and the wall clock up with it. The smallest
    //    shape is never pruned, so this can never empty the set on its own.
    let smallest = universe.iter().map(|s| s.nonces()).min().unwrap_or(1);
    let batch_ceiling = ((P95_BATCH_CEILING_MS / 1000.0) * LATENCY_HEADROOM * probe_hps) as u64;
    let batch_ceiling = batch_ceiling.max(smallest);
    let (affordable, over_ceiling): (Vec<Shape>, Vec<Shape>) = universe
        .iter()
        .copied()
        .partition(|shape| shape.nonces() <= batch_ceiling);

    // 2. How long a pass may be. Two independent ceilings, and the plan takes
    //    the lower: the sweep has to fit the operator's budget, and the soak has
    //    to be able to settle on the corpus the sweep chose.
    let expected_passes = coarse.len() as u32 + REFINE_PASS_ALLOWANCE + FINAL_PASS_ALLOWANCE;
    let sweep_pass_seconds =
        budget_seconds as f64 * SWEEP_BUDGET_SHARE / expected_passes as f64;
    let soak_pass_seconds = max_soak_pass_seconds(budget_seconds) * SOAK_PASS_MARGIN;
    let pass_ceiling_seconds = sweep_pass_seconds.min(soak_pass_seconds);

    // The corpus also has to fit the 32-bit nonce space it is placed in, with
    // room for several segments above the base.
    let space_cap = (u32::MAX as u64 - nonce_start as u64) / 4;
    let quantum_cap = ((pass_ceiling_seconds * probe_hps) as u64)
        .max(MIN_SEGMENT_NONCES)
        .min(space_cap);

    let (mut corpus, usable, off_corpus) = plan_corpus(
        &affordable,
        nonce_start,
        headers,
        1,
        MIN_SEGMENT_NONCES,
        quantum_cap,
    )
    .map_err(|error| {
        format!(
            "{error}. At the probed {:.2} MH/s a pass may last {:.1}s, which is {} nonces; the \
             smallest launch this device offers is {} nonces. Lower [gpu] work_groups, or raise \
             [efficiency] benchmark_seconds",
            probe_hps / 1e6,
            pass_ceiling_seconds,
            quantum_cap,
            smallest
        )
    })?;

    // 3. Segments. One is the floor, not four: the header count follows the
    //    segments rather than forcing them.
    let segment = corpus.segment_nonces.max(1);
    let want_nonces = (sweep_pass_seconds * probe_hps).max(segment as f64);
    let space_segments = ((u32::MAX as u64 - nonce_start as u64) / segment).max(1);
    let soak_segments = (((soak_pass_seconds * probe_hps) as u64) / segment).max(1);
    corpus.segments = (want_nonces / segment as f64).round().max(1.0).min(64.0) as u64 as u32;
    corpus.segments = corpus
        .segments
        .min(space_segments.min(soak_segments).min(u32::MAX as u64) as u32)
        .max(1);
    corpus.headers = headers.max(1).min(corpus.segments);

    let candidates: Vec<Shape> = coarse
        .iter()
        .copied()
        .filter(|shape| usable.contains(shape))
        .collect();

    let pass_seconds = corpus.total_nonces() as f64 / probe_hps;
    let sweep_passes =
        candidates.len() as u32 + REFINE_PASS_ALLOWANCE + FINAL_PASS_ALLOWANCE;

    let plan = SessionPlan {
        corpus,
        candidates,
        usable,
        over_ceiling,
        off_corpus,
        probe_hps,
        pass_seconds,
        pass_ceiling_seconds,
        sweep_passes,
        sweep_seconds: pass_seconds * sweep_passes as f64,
        budget_for_every_shape: budget_for_every_shape(
            &affordable,
            probe_hps,
            expected_passes,
            space_cap,
        ),
    };

    if !plan.is_a_comparison() {
        return Err(format!(
            "only {} launch shape survived planning ({} could not meet the {:.0} ms batch ceiling \
             at {:.2} MH/s, {} would not fit a {:.1}s pass). A tune of one shape is a report, not \
             a comparison. Lower [gpu] work_groups so the window starts smaller, or raise \
             [efficiency] benchmark_seconds",
            plan.candidates.len(),
            plan.over_ceiling.len(),
            P95_BATCH_CEILING_MS,
            probe_hps / 1e6,
            plan.off_corpus.len(),
            pass_ceiling_seconds,
        ));
    }
    // Belt and braces on the arithmetic this whole section exists to enforce.
    // If it ever fails, the tuner must say so here rather than after forty
    // minutes of sweeping followed by "the card never settled".
    if !plan.soak_can_settle(budget_seconds) {
        return Err(format!(
            "one pass of the planned corpus takes {:.1}s at {:.2} MH/s, and a soak can only settle \
             on passes under {:.1}s, so this tune could sweep for {:.0}s and still never settle. \
             Raise [efficiency] benchmark_seconds, or lower [gpu] work_groups",
            plan.pass_seconds,
            probe_hps / 1e6,
            max_soak_pass_seconds(budget_seconds),
            plan.sweep_seconds,
        ));
    }
    Ok(plan)
}

/// The `benchmark_seconds` at which no shape would be dropped for cost.
///
/// Both ceilings have to clear the full quantum: the sweep's share of the budget
/// spread over the passes it will make, and the soak's settling window. The soak
/// cap saturates at 900 s, so beyond a certain quantum no budget buys the shapes
/// back and the answer is `None` rather than a number that would not work.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
fn budget_for_every_shape(
    shapes: &[Shape],
    probe_hps: f64,
    expected_passes: u32,
    space_cap: u64,
) -> Option<u64> {
    let quantum = segment_for(shapes, MIN_SEGMENT_NONCES, space_cap)?;
    let seconds = quantum as f64 / probe_hps;
    let for_sweep = seconds * expected_passes as f64 / SWEEP_BUDGET_SHARE;
    // soak_cap(b) / (w - 1) * margin >= seconds, and soak_cap(b) = b / 2 once
    // the budget is over the 180 s the 90 s floor covers.
    let cap_needed = seconds * (soak_window_passes() as f64 - 1.0) / SOAK_PASS_MARGIN;
    if cap_needed > 900.0 {
        return None;
    }
    let for_soak = cap_needed * 2.0;
    Some(for_sweep.max(for_soak).ceil() as u64)
}

// ---------------------------------------------------------------------------
// What a candidate is judged on
// ---------------------------------------------------------------------------

/// Hacash targets one block every 300 seconds.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub const TARGET_BLOCK_SECONDS: f64 = 300.0;

/// The p95 batch-latency ceiling, and where the number comes from.
///
/// A batch cannot be interrupted: once it is enqueued, a template change cannot
/// take effect until it returns. If the job changes at a uniformly random moment
/// inside a batch of L seconds, L/2 seconds of hashing is thrown away, and at a
/// 300-second block target that is L/2/300 of the miner's output. 1.5 s holds
/// that expected loss under 0.25% while leaving room for the largest launch this
/// card can make. It is a ceiling on the p95 rather than the mean because what
/// hurts is the tail: a shape whose worst batches take four seconds delays every
/// template change by four seconds however good its average is.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub const P95_BATCH_CEILING_MS: f64 = 1_500.0;

/// Fraction of hashing thrown away by template changes, for a given mean batch.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn stale_fraction(mean_batch_seconds: f64) -> f64 {
    if !mean_batch_seconds.is_finite() || mean_batch_seconds <= 0.0 {
        return 0.0;
    }
    (mean_batch_seconds / 2.0 / TARGET_BLOCK_SECONDS).clamp(0.0, 1.0)
}

/// Raw hashrate discounted by the work template changes will throw away. This,
/// not the raw figure, is what "sustained valid H/s" means.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn sustained_valid_hps(hashrate: f64, mean_batch_seconds: f64) -> f64 {
    if !hashrate.is_finite() || hashrate <= 0.0 {
        return 0.0;
    }
    hashrate * (1.0 - stale_fraction(mean_batch_seconds))
}

/// Where the watts in a score came from. Printed next to every number, because
/// a measured 291 W and a configured 350 W lead to different winners and the
/// operator has to be able to tell which one picked theirs.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WattsSource {
    /// The card's own board-power sensor.
    Measured,
    /// The configured `gpu_watts` scaled by the profile and the launch size.
    Estimated,
}

#[cfg(any(feature = "ocl", feature = "cuda", test))]
impl WattsSource {
    pub fn label(self) -> &'static str {
        match self {
            WattsSource::Measured => "measured",
            WattsSource::Estimated => "estimated",
        }
    }
}

/// The prices a Profit score needs, and the one quantity that is not a price:
/// how much HAC a hash per second earns in a day at the current difficulty.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
#[derive(Clone, Copy, Debug, Default)]
pub struct Economics {
    pub power_cost_kwh: f64,
    pub hac_price: f64,
    /// HAC per day per H/s at the network's current target. `None` when the
    /// tuner could not learn the difficulty, which is not the same as zero.
    pub hac_per_hps_day: Option<f64>,
    /// Draw of the CPU threads that assist the GPU while mining. Always an
    /// estimate; there is no per-core power sensor.
    pub cpu_watts: f64,
}

#[cfg(any(feature = "ocl", feature = "cuda", test))]
impl Economics {
    pub fn eur_per_hps_day(&self) -> Option<f64> {
        let hac = self.hac_per_hps_day?;
        (hac > 0.0 && self.hac_price > 0.0).then(|| hac * self.hac_price)
    }

    pub fn daily_power_cost_eur(&self, watts: f64) -> f64 {
        watts * 24.0 / 1000.0 * self.power_cost_kwh
    }
}

/// What the tuner is actually maximising.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Objective {
    /// Sustained valid hashes per second.
    ValidHashrate,
    /// Sustained valid hashes per joule.
    HashesPerJoule,
    /// Net EUR per day: block-reward revenue minus electricity.
    NetIncome,
}

#[cfg(any(feature = "ocl", feature = "cuda", test))]
impl Objective {
    pub fn label(self) -> &'static str {
        match self {
            Objective::ValidHashrate => "sustained valid H/s",
            Objective::HashesPerJoule => "sustained valid H/J",
            Objective::NetIncome => "net EUR/day",
        }
    }
}

/// Turn the operator's mode into something that can actually be computed from
/// what this rig measures, and say so when that is not what they asked for.
///
/// Profit is the one that can fail: net income needs a price for a hash, which
/// needs the network's difficulty and a HAC price. Without both, net is not
/// merely inaccurate, it is undefined, and the honest fallback is the one that
/// maximises revenue: throughput. Silently ranking on kH/J instead would answer
/// a different question and never say it did.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn resolve_objective(
    mode: EfficiencyMode,
    econ: &Economics,
) -> (Objective, Option<&'static str>) {
    match mode {
        EfficiencyMode::Max => (Objective::ValidHashrate, None),
        EfficiencyMode::Eco => (Objective::HashesPerJoule, None),
        EfficiencyMode::Profit => {
            if econ.power_cost_kwh <= 0.0 {
                return (
                    Objective::ValidHashrate,
                    Some("power_cost_kwh is not set, so electricity is free and net income is maximised by throughput"),
                );
            }
            match econ.eur_per_hps_day() {
                Some(_) => (Objective::NetIncome, None),
                None if econ.hac_price <= 0.0 => (
                    Objective::ValidHashrate,
                    Some("hac_price is not set, so revenue has no value and net income cannot be ranked; ranking on throughput instead"),
                ),
                None => (
                    Objective::ValidHashrate,
                    Some("the network difficulty could not be read, so the value of a hash is unknown; ranking on throughput instead"),
                ),
            }
        }
    }
}

/// Everything the score of one candidate is computed from.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
#[derive(Clone, Copy, Debug)]
pub struct ScoreInput {
    pub hashrate: f64,
    pub mean_batch_seconds: f64,
    pub p95_batch_ms: f64,
    /// Board draw while this candidate was running.
    pub gpu_watts: f64,
}

#[cfg(any(feature = "ocl", feature = "cuda", test))]
impl ScoreInput {
    pub fn valid_hps(&self) -> f64 {
        sustained_valid_hps(self.hashrate, self.mean_batch_seconds)
    }

    pub fn total_watts(&self, econ: &Economics) -> f64 {
        self.gpu_watts + econ.cpu_watts
    }

    /// Sustained valid hashes per joule.
    pub fn hashes_per_joule(&self, econ: &Economics) -> f64 {
        let watts = self.total_watts(econ);
        if watts <= 0.0 {
            return 0.0;
        }
        self.valid_hps() / watts
    }

    /// Net EUR per day, or `None` when a hash has no known value.
    pub fn net_eur_per_day(&self, econ: &Economics) -> Option<f64> {
        let value = econ.eur_per_hps_day()?;
        Some(self.valid_hps() * value - econ.daily_power_cost_eur(self.total_watts(econ)))
    }

    /// A candidate whose worst batches stall the miner past the ceiling is
    /// refused, whatever it scores.
    pub fn within_latency_ceiling(&self, ceiling_ms: f64) -> bool {
        self.p95_batch_ms.is_finite() && self.p95_batch_ms <= ceiling_ms
    }
}

/// Score one candidate. Higher is better in every objective.
///
/// `None` means the candidate is not admissible at all: a measurement that is
/// not finite and positive, or a shape that blows the latency ceiling.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn score(input: &ScoreInput, objective: Objective, econ: &Economics, ceiling_ms: f64) -> Option<f64> {
    if !input.hashrate.is_finite() || input.hashrate <= 0.0 {
        return None;
    }
    if !input.within_latency_ceiling(ceiling_ms) {
        return None;
    }
    let value = match objective {
        Objective::ValidHashrate => input.valid_hps(),
        Objective::HashesPerJoule => input.hashes_per_joule(econ),
        Objective::NetIncome => input.net_eur_per_day(econ)?,
    };
    value.is_finite().then_some(value)
}

// ---------------------------------------------------------------------------
// Settling
// ---------------------------------------------------------------------------

/// One pass of the corpus during the soak, with the telemetry taken while it ran.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
#[derive(Clone, Copy, Debug, Default)]
pub struct SoakPass {
    pub seconds: f64,
    pub hashrate: f64,
    /// The pass's p95 BATCH latency, which is the quantity the miner's stale
    /// work is priced from and the one that moves when a card starts throttling
    /// under a sustained load. Recorded per pass rather than only for the last
    /// one, so a soak that settles on hashrate while its tail grows is visible.
    pub p95_ms: f64,
    pub temp_c: Option<f32>,
    pub watts: Option<f32>,
    pub clock_mhz: Option<f32>,
}

/// How flat is flat enough.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
#[derive(Clone, Copy, Debug)]
pub struct SettleLimits {
    /// Passes that must all be flat together.
    pub window: usize,
    pub temp_span_c: f64,
    pub watts_span_pct: f64,
    pub clock_span_pct: f64,
    pub rate_span_pct: f64,
}

#[cfg(any(feature = "ocl", feature = "cuda", test))]
impl Default for SettleLimits {
    /// The numbers are the card's own noise, not aspirations. Within one process
    /// the fixed-corpus baseline reproduces to about 0.3%, so a 1% hashrate span
    /// is comfortably above the measurement floor and still tight enough that a
    /// card still climbing its clock ramp cannot pass. 1 C is the resolution the
    /// driver reports temperature in.
    fn default() -> Self {
        SettleLimits {
            window: 5,
            temp_span_c: 1.0,
            watts_span_pct: 3.0,
            clock_span_pct: 2.0,
            rate_span_pct: 1.0,
        }
    }
}

/// The spans measured over the settling window, and whether they are all inside
/// the limits. Sensors the card does not have are `None` and are not required.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
#[derive(Clone, Copy, Debug, Default)]
pub struct SettleState {
    pub passes: usize,
    pub rate_span_pct: f64,
    pub temp_span_c: Option<f64>,
    pub watts_span_pct: Option<f64>,
    pub clock_span_pct: Option<f64>,
    pub settled: bool,
}

#[cfg(any(feature = "ocl", feature = "cuda", test))]
impl SettleState {
    /// The signals this card never reported, so "settled" can be read for what
    /// it is worth.
    ///
    /// `settle_state` treats an absent sensor as satisfied, and it has to: a
    /// card with no board-power sensor would otherwise soak until the cap on
    /// every run. But "settled" over one signal and "settled" over four are
    /// different claims, and a report that printed only the spans it had let the
    /// weaker one wear the stronger one's word. Named here, printed there.
    pub fn absent_signals(&self) -> Vec<&'static str> {
        let mut absent = Vec::new();
        if self.temp_span_c.is_none() {
            absent.push("temperature");
        }
        if self.watts_span_pct.is_none() {
            absent.push("board power");
        }
        if self.clock_span_pct.is_none() {
            absent.push("shader clock");
        }
        absent
    }
}

#[cfg(any(feature = "ocl", feature = "cuda", test))]
fn span(values: &[f64]) -> Option<(f64, f64)> {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for value in values {
        if !value.is_finite() {
            return None;
        }
        lo = lo.min(*value);
        hi = hi.max(*value);
    }
    (lo.is_finite() && hi.is_finite()).then_some((lo, hi))
}

#[cfg(any(feature = "ocl", feature = "cuda", test))]
fn span_pct(values: &[f64]) -> Option<f64> {
    let (lo, hi) = span(values)?;
    let mid = (lo + hi) / 2.0;
    (mid > 0.0).then(|| (hi - lo) / mid * 100.0)
}

/// Is the last `limits.window` passes' worth of telemetry flat?
///
/// A sensor that is absent on every pass is not a reason to refuse to settle:
/// an NVIDIA card with no board-power sensor would otherwise soak forever. A
/// sensor that is present must be flat.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn settle_state(passes: &[SoakPass], limits: &SettleLimits) -> SettleState {
    let window = limits.window.max(2);
    if passes.len() < window {
        return SettleState {
            passes: passes.len(),
            settled: false,
            ..SettleState::default()
        };
    }
    let tail = &passes[passes.len() - window..];
    let rates: Vec<f64> = tail.iter().map(|pass| pass.hashrate).collect();
    let rate_span_pct = span_pct(&rates).unwrap_or(f64::INFINITY);

    let collect = |pick: fn(&SoakPass) -> Option<f32>| -> Option<Vec<f64>> {
        let values: Vec<f64> = tail.iter().filter_map(|p| pick(p)).map(f64::from).collect();
        (values.len() == tail.len()).then_some(values)
    };
    let temps = collect(|p| p.temp_c);
    let watts = collect(|p| p.watts);
    let clocks = collect(|p| p.clock_mhz);

    let temp_span_c = temps.as_deref().and_then(span).map(|(lo, hi)| hi - lo);
    let watts_span_pct = watts.as_deref().and_then(span_pct);
    let clock_span_pct = clocks.as_deref().and_then(span_pct);

    let ok = |measured: Option<f64>, limit: f64| measured.map(|v| v <= limit).unwrap_or(true);
    let settled = rate_span_pct <= limits.rate_span_pct
        && ok(temp_span_c, limits.temp_span_c)
        && ok(watts_span_pct, limits.watts_span_pct)
        && ok(clock_span_pct, limits.clock_span_pct);

    SettleState {
        passes: passes.len(),
        rate_span_pct,
        temp_span_c,
        watts_span_pct,
        clock_span_pct,
        settled,
    }
}

// ---------------------------------------------------------------------------
// The operator's temperature ceiling
// ---------------------------------------------------------------------------

/// What `[efficiency] max_temp_c` is actually doing on this card.
///
/// Three states rather than two, because "no ceiling was asked for" and "a
/// ceiling was asked for and cannot be enforced" are opposite situations that a
/// boolean would merge. The second is the one that used to be invisible: on a
/// card with no temperature source, `within_temperature_limit` saw an absent
/// peak, had nothing to compare, and returned `Ok`, so a ceiling the operator
/// set was silently satisfied by every shape including the one that cooks the
/// card. Absent is now its own state and it is said out loud.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TempCeiling {
    /// `max_temp_c` is 0 or unset: nothing to enforce, and nothing to warn about.
    NotRequested,
    /// A ceiling is set and this machine reports this card's temperature.
    Enforced { limit_c: f32 },
    /// A ceiling is set and nothing on this machine reports this card's
    /// temperature, so it cannot be enforced at all.
    Unenforceable { limit_c: f32 },
}

#[cfg(any(feature = "ocl", feature = "cuda", test))]
impl TempCeiling {
    /// Resolve the state from the operator's setting and what the card offers.
    pub fn resolve(max_temp_c: Option<f32>, sensor_reports_temperature: bool) -> TempCeiling {
        match max_temp_c {
            None => TempCeiling::NotRequested,
            Some(limit_c) if sensor_reports_temperature => TempCeiling::Enforced { limit_c },
            Some(limit_c) => TempCeiling::Unenforceable { limit_c },
        }
    }

    pub fn is_enforceable(self) -> bool {
        !matches!(self, TempCeiling::Unenforceable { .. })
    }

    /// One line for the tune's log and one for its report, in the operator's
    /// terms: the setting they typed and what it is worth here.
    pub fn describe(self, sensor: &str) -> String {
        match self {
            TempCeiling::NotRequested => format!(
                "no ceiling set ([efficiency] max_temp_c = 0), sensor: {sensor}"
            ),
            TempCeiling::Enforced { limit_c } => format!(
                "{limit_c:.0} C ceiling from [efficiency] max_temp_c, enforced against {sensor}"
            ),
            TempCeiling::Unenforceable { limit_c } => format!(
                "{limit_c:.0} C ceiling from [efficiency] max_temp_c CANNOT BE ENFORCED: {sensor}"
            ),
        }
    }
}

/// What one measurement window is worth against the ceiling.
///
/// `NotMeasured` is deliberately not `Under`: a window that reported no
/// temperature has not been checked, and calling that a pass is exactly the
/// silent no-op this replaces. The session-level guard is `TempCeiling`, which
/// refuses before the sweep when the card has no sensor at all; a `NotMeasured`
/// after that guard is a sampling gap in one short window, which the caller
/// reports rather than treats as proof of anything.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TempWindow {
    /// No ceiling was asked for, so there is nothing to check.
    NoCeiling,
    /// A peak was measured and it is at or under the ceiling.
    Under { peak_c: f32, limit_c: f32 },
    /// A ceiling is set and this window carries no temperature at all.
    NotMeasured { limit_c: f32 },
}

/// Judge one window's peak against the operator's ceiling.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn temp_window_state(
    peak_c: Option<f32>,
    max_temp_c: Option<f32>,
) -> Result<TempWindow, String> {
    let Some(limit_c) = max_temp_c else {
        return Ok(TempWindow::NoCeiling);
    };
    let Some(peak_c) = peak_c else {
        return Ok(TempWindow::NotMeasured { limit_c });
    };
    if peak_c > limit_c {
        return Err(format!(
            "reached {peak_c:.0} C, above the {limit_c:.0} C ceiling set in [efficiency] max_temp_c"
        ));
    }
    Ok(TempWindow::Under { peak_c, limit_c })
}

// ---------------------------------------------------------------------------
// Percentiles
// ---------------------------------------------------------------------------

/// Nearest-rank percentile of an ascending sample.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (quantile.clamp(0.0, 1.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    if sorted.is_empty() {
        return 0.0;
    }
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

// ---------------------------------------------------------------------------
// Sizing one device allocation to serve several candidates
// ---------------------------------------------------------------------------

/// The work-group count one device must be ALLOCATED with so that it can launch
/// `shape` and every other planned shape sharing `shape`'s unit_size.
///
/// This exists for the backends where `unit_size` is baked into the allocation
/// and the kernel reads it from the miner (CUDA), so a candidate with a new
/// unit_size needs a new device while work groups can be clamped per launch.
/// Opening one device per unit_size instead of one per candidate is the
/// difference between three device opens in a sweep and twenty-four, and each
/// open costs an allocation, a kernel self-test and two warm-up batches.
///
/// The rule is per unit_size and NOT "the largest shape in the plan", and the
/// difference is a real allocation failure rather than a nicety: a plan holding
/// both 3072x256x32 and 256x256x128 would, on the naive rule, allocate
/// 3072x256x128 - 100 M nonces, 3.6 GB - for a launch nobody was ever going to
/// make. Filtering by unit_size bounds the allocation by the largest batch the
/// plan actually contains.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn shared_allocation_work_groups(plan: &[Shape], shape: Shape) -> u32 {
    plan.iter()
        .filter(|planned| {
            planned.local_size == shape.local_size && planned.unit_size == shape.unit_size
        })
        .map(|planned| planned.work_groups)
        .max()
        .unwrap_or(shape.work_groups)
        .max(shape.work_groups)
}

// ---------------------------------------------------------------------------
// How well the final round can actually tell two shapes apart
// ---------------------------------------------------------------------------

/// One finalist's repeated passes in the final round.
///
/// The final round runs the finalists in alternating order and takes each one's
/// median, which cancels the drift that would otherwise hand the win to whichever
/// shape happened to run while the card was cool. What it does NOT do by itself
/// is say how far apart two medians have to be before the difference is real,
/// and until now nothing did: a tune could report a 0.2% win over a card whose
/// own repeats spanned 3% and the report would read exactly like a 20% win.
///
/// The passes needed for that number are already being run. This keeps them.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
#[derive(Clone, Debug)]
pub struct FinalistRuns {
    pub shape: Shape,
    /// The objective's value on each pass, in the order the passes ran.
    pub scores: Vec<f64>,
}

#[cfg(any(feature = "ocl", feature = "cuda", test))]
impl FinalistRuns {
    pub fn median(&self) -> f64 {
        median(&self.scores)
    }

    /// Peak-to-peak span of this shape's OWN repeated passes, as a percentage of
    /// its median. Identical work, identical shape, same card, minutes apart: a
    /// non-zero span here is pure measurement noise.
    pub fn span_pct(&self) -> f64 {
        let centre = self.median();
        if self.scores.len() < 2 || centre <= 0.0 {
            return 0.0;
        }
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for value in &self.scores {
            if !value.is_finite() {
                return f64::INFINITY;
            }
            lo = lo.min(*value);
            hi = hi.max(*value);
        }
        (hi - lo) / centre * 100.0
    }
}

/// The resolution of the final round: the largest noise span any one finalist
/// showed on its own repeated passes.
///
/// A margin between two DIFFERENT shapes that is smaller than the spread one
/// shape shows against itself has not been demonstrated. This is the same
/// argument `x16rs_gate ab` makes with its paired p10-p90, measured here from
/// the passes the tuner runs anyway rather than assumed from a constant.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn comparison_resolution_pct(finalists: &[FinalistRuns]) -> f64 {
    finalists
        .iter()
        .filter(|f| f.scores.len() >= 2)
        .map(|f| f.span_pct())
        .fold(0.0f64, f64::max)
}

/// The winner's margin over the runner-up, as a percentage of the runner-up.
/// `None` when fewer than two finalists produced a score.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn winning_margin_pct(finalists: &[FinalistRuns]) -> Option<f64> {
    let mut medians: Vec<f64> = finalists
        .iter()
        .filter(|f| !f.scores.is_empty())
        .map(|f| f.median())
        .collect();
    if medians.len() < 2 {
        return None;
    }
    medians.sort_by(|a, b| b.total_cmp(a));
    (medians[1] > 0.0).then(|| (medians[0] - medians[1]) / medians[1] * 100.0)
}

/// The sentence a tune has to be able to say about its own answer.
///
/// Three claims, and the third is the one that only became sayable when the
/// tuner learned to measure CUDA:
///
///   * what the choice between finalists was resolved to, MEASURED from the
///     finalists' own repeats;
///   * whether the winner's margin cleared it;
///   * that the absolute hashrate above is a number from THIS process, and two
///     processes on this rig have disagreed by
///     [`crate::x16rs_gate::BETWEEN_PROCESS_SPREAD_PCT`] on identical work.
///
/// The third matters more on CUDA than on OpenCL and the note says why. Two
/// OpenCL KERNEL trees can be alternated inside one process (`x16rs_gate ab`)
/// because OpenCL compiles kernels at runtime from a directory; nvcc compiles
/// CUDA kernels into the binary, so two CUDA kernel builds are two binaries and
/// can only ever be compared across processes. Two launch SHAPES are a different
/// question and are compared in-process on both backends, which is what the
/// first two claims are about - the tuner does not inherit the between-process
/// figure just because it is running on CUDA.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn resolution_note(finalists: &[FinalistRuns], backend: &str) -> String {
    let resolution = comparison_resolution_pct(finalists);
    let mut text = format!(
        "  resolution       : the finalists were re-run in alternating order inside ONE process; \
         the widest\n                     spread any single shape showed against itself was \
         {resolution:.2}%, which is what this\n                     comparison can resolve"
    );
    match winning_margin_pct(finalists) {
        Some(margin) if margin >= resolution && resolution > 0.0 => text.push_str(&format!(
            ".\n                     The winner beat the runner-up by {margin:.2}%, which clears it"
        )),
        Some(margin) => text.push_str(&format!(
            ".\n                     The winner beat the runner-up by only {margin:.2}%, which does \
             NOT clear it: these two\n                     shapes were not told apart, and either \
             would do"
        )),
        None => text.push_str(
            ".\n                     Only one finalist produced a score, so nothing was compared \
             in this round",
        ),
    }
    text.push_str(&format!(
        "\n  across runs      : the hashrate above is this process's. Separate runs of this binary \
         on\n                     identical work have disagreed by ~{:.1}% on this rig, so a number \
         from\n                     another run is not comparable with it below that.",
        crate::x16rs_gate::BETWEEN_PROCESS_SPREAD_PCT
    ));
    if backend == "cuda" {
        text.push_str(
            "\n                     For CUDA that bound also applies to KERNEL changes and cannot \
             be beaten:\n                     nvcc compiles the kernel into the binary, so two \
             kernel builds are two\n                     binaries and there is no in-process A/B \
             for them the way `x16rs_gate ab`\n                     alternates two OpenCL kernel \
             trees. Launch shapes, which is what this\n                     tune compares, are \
             alternated in-process and resolve as stated above.",
        );
    }
    text.push('\n');
    text
}

// ---------------------------------------------------------------------------
// The candidate grid
// ---------------------------------------------------------------------------

/// Every value on both tuning axes is 2^a or 3 * 2^a, and nothing else.
///
/// That is not aesthetics, it is what makes a shared corpus possible at all.
/// The corpus segment must be a common multiple of every candidate's batch
/// size; with both axes of this form, every batch is 2^k * 3^j with j at most 2,
/// so the segment is at most nine times the largest batch. Admit one 112
/// (2^4 * 7) or one 40 (2^3 * 5) and the segment jumps by a factor of 7 or 5,
/// which either makes every measurement seven times longer than it needs to be
/// or forces candidates out of the comparison. A grid step of roughly 1.5x is
/// also finer than this kernel's response to either axis, so nothing is lost by
/// it: on the 9070 XT the whole work-group axis is 32, 48, 64.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn dyadic_grid(min: u32, max: u32) -> Vec<u32> {
    let min = min.max(1);
    let max = max.max(min);
    let mut out = Vec::new();
    let mut power = 1u32;
    loop {
        for value in [power, power.saturating_mul(3)] {
            if value >= min && value <= max && !out.contains(&value) {
                out.push(value);
            }
        }
        let Some(next) = power.checked_mul(2) else {
            break;
        };
        if next > max {
            break;
        }
        power = next;
    }
    if out.is_empty() {
        // A window so narrow that it contains no grid point at all. The cap
        // itself is then the only candidate: it is the shape the device can
        // really run, and one point measured is better than none.
        out.push(max);
    }
    out.sort_unstable();
    out
}

/// Unit sizes, before clamping to the device.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn unit_size_grid(max_unit_size: u32) -> Vec<u32> {
    dyadic_grid(32, max_unit_size.max(32))
}

/// Work-group counts, before clamping to the device.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn work_group_grid(min_wg: u32, max_wg: u32) -> Vec<u32> {
    dyadic_grid(min_wg, max_wg)
}

/// Every other point of a full axis, and always its top end.
///
/// This is what makes a coarse pass coarse: the sweep visits half the grid, and
/// the refinement fills in the neighbours of whatever won, so a full product
/// sweep is never paid for.
///
/// # Which half, and why it is not a free choice
///
/// The dyadic grid alternates `2^k` and `3 * 2^k`, so taking every other point
/// takes one of those two families whole. Both cover the axis equally after
/// refinement, but they cost wildly different amounts to MEASURE: the shared
/// corpus segment is a common multiple of every candidate's batch, so a family
/// of pure powers of two gives a segment equal to the largest batch, while one
/// 3-multiple multiplies it by three. `plan_corpus` then has to drop shapes to
/// fit the budget, and it drops the 3-multiples first, for exactly that reason.
///
/// Starting at index 0 unconditionally leaves which family is chosen to the
/// arbitrary question of whether the grid's bottom end happened to be cut by
/// `min`. It is 32 on an RX 9070 XT, so index 0 is a power of two and the sweep
/// is cheap. It is 48 on a Tesla T4 - whose 40 multiprocessors put the floor
/// between 32 and 48 - so index 0 is 48, every coarse candidate is a
/// 3-multiple, `plan_corpus` drops every one of them to fit the corpus, and the
/// tune ends with "only 0 launch shapes survived planning" on a card that has
/// nothing wrong with it. That is not a hypothetical: it is what a 90-second
/// budget does on the one NVIDIA card this kernel has been measured on.
///
/// So the offset is chosen rather than assumed: whichever of the two families
/// contains more powers of two. Ties keep index 0, so nothing already measured
/// moves, and the axis covered is identical either way.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
fn coarse_axis(full: &[u32]) -> Vec<u32> {
    let family = |offset: usize| -> Vec<u32> {
        let mut out: Vec<u32> = full.iter().copied().skip(offset).step_by(2).collect();
        if let Some(last) = full.last() {
            if !out.contains(last) {
                out.push(*last);
            }
        }
        out.sort_unstable();
        out
    };
    let powers_of_two = |axis: &[u32]| axis.iter().filter(|v| v.is_power_of_two()).count();
    let evens = family(0);
    let odds = family(1);
    if powers_of_two(&odds) > powers_of_two(&evens) {
        odds
    } else {
        evens
    }
}

/// The coarse candidate set for a device.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn coarse_candidates(
    min_wg: u32,
    max_wg: u32,
    max_unit_size: u32,
    local_size: u32,
) -> Vec<Shape> {
    let mut out = Vec::new();
    for work_groups in coarse_axis(&work_group_grid(min_wg, max_wg)) {
        for unit_size in coarse_axis(&unit_size_grid(max_unit_size)) {
            out.push(Shape {
                work_groups,
                local_size,
                unit_size,
            });
        }
    }
    out
}

/// The full product grid: everything the coarse sweep or a refinement could
/// ever ask for.
///
/// The corpus is planned over this once per session, minus whatever
/// `plan_session` prunes from it, so a finalist's neighbours are measured on the
/// same corpus as the coarse sweep rather than on one rebuilt around them. What
/// survives the prune is `SessionPlan::usable`, and refinement draws only from
/// there.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn candidate_universe(
    min_wg: u32,
    max_wg: u32,
    max_unit_size: u32,
    local_size: u32,
) -> Vec<Shape> {
    let mut out = Vec::new();
    for work_groups in work_group_grid(min_wg, max_wg) {
        for unit_size in unit_size_grid(max_unit_size) {
            out.push(Shape {
                work_groups,
                local_size,
                unit_size,
            });
        }
    }
    out
}

/// The immediate neighbours of a shape on the full grid: the points the coarse
/// sweep skipped.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn refine_candidates(base: Shape, min_wg: u32, max_wg: u32, max_unit_size: u32) -> Vec<Shape> {
    let neighbours = |grid: &[u32], value: u32| -> Vec<u32> {
        let Some(index) = grid.iter().position(|entry| *entry == value) else {
            return Vec::new();
        };
        [index.checked_sub(1), index.checked_add(1)]
            .into_iter()
            .flatten()
            .filter_map(|i| grid.get(i).copied())
            .collect()
    };
    let mut out = vec![base];
    for unit_size in neighbours(&unit_size_grid(max_unit_size), base.unit_size) {
        out.push(Shape { unit_size, ..base });
    }
    for work_groups in neighbours(&work_group_grid(min_wg, max_wg), base.work_groups) {
        out.push(Shape {
            work_groups,
            ..base
        });
    }
    out.sort_by_key(|shape| (shape.work_groups, shape.unit_size));
    out.dedup();
    out
}

/// Name the winning shape with the profile tier it sits closest to, so the ini
/// the panel reads keeps meaning what it meant.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn profile_for_shape(
    vendor: crate::gpu_arch::GpuVendor,
    shape: Shape,
    max_wg: u32,
    max_unit_size: u32,
) -> String {
    let ceiling = (max_wg as f64 * max_unit_size as f64).max(1.0);
    let load = (shape.work_groups as f64 * shape.unit_size as f64 / ceiling).clamp(0.0, 1.0);
    let tier = match load {
        l if l < 0.20 => 0,
        l if l < 0.40 => 1,
        l if l < 0.65 => 2,
        l if l < 0.90 => 3,
        _ => 4,
    };
    crate::efficiency::tier_profile_for_vendor(vendor, tier).to_string()
}

#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn pick_for_shape(
    vendor: crate::gpu_arch::GpuVendor,
    shape: Shape,
    max_wg: u32,
    max_unit_size: u32,
) -> BenchmarkPick {
    BenchmarkPick {
        profile: profile_for_shape(vendor, shape, max_wg, max_unit_size),
        workgroups: shape.work_groups,
        unitsize: shape.unit_size,
    }
}

// ===========================================================================
// Everything below needs a real device.
// ===========================================================================

/// The device half of the tuner: one implementation, both backends.
///
/// Nothing below this line is written twice. `x16rs_gate` already reduced a GPU
/// backend to two traits - a way to open a device at a launch shape
/// ([`crate::x16rs_gate::GateBackend`]) and three operations on the device
/// itself ([`crate::x16rs_gate::GateDevice`]) - and proved that the corpus, the
/// CPU oracle, the threshold arithmetic and the comparison do not need to know
/// which card they are running on. A tuner needs exactly those three operations
/// and nothing more: `best` to time a candidate, `count_and_shares` to prove it,
/// and `best` again for the reduction solo mining reads.
///
/// So this module is generic over the same two traits, and the CUDA tuner is not
/// a second tuner. The corpus, the scoring, the latency ceiling, the soak, the
/// settling test, the temperature ceiling and the blame for a wrong hash are the
/// SAME CODE for an RX 9070 XT and a Tesla T4, which is the only way two cards'
/// answers can be compared at all.
///
/// # What the two backends really do differ in, and where it is handled
///
/// One thing, and it is not a preference: an OpenCL device takes its launch
/// shape per call, so one allocation sized at the top of the grid serves every
/// candidate under it, while a CUDA miner has `unit_size` baked into its device
/// buffers AND passed to the kernel from the miner struct, so a candidate with a
/// different `unit_size` needs a different miner. That is the whole difference,
/// it is answered by `GateBackend::device_is_bound_to_its_shape` and
/// `GateDevice::can_launch`, and [`Devices`] is the twenty lines that act on it.
#[cfg(any(feature = "ocl", feature = "cuda"))]
mod device {
    use super::*;
    use crate::x16rs_gate::{
        GateBackend, GateDevice, REPEAT16_HEIGHT, SHARE_LIST_CAPACITY, corpus_header, cpu_hash,
        cpu_hash_window, threshold_miss_probability, threshold_ranks,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// Telemetry sampled while a candidate ran.
    #[derive(Clone, Debug, Default)]
    pub struct TelemetryWindow {
        pub temp_c: Option<f32>,
        /// The hottest single sample, not the mean. A shape that spends one
        /// second above the operator's limit has been above it.
        pub peak_temp_c: Option<f32>,
        pub watts: Option<f32>,
        pub clock_mhz: Option<f32>,
        pub samples: usize,
    }

    /// A background sampler bound to one card for the life of a tuning session.
    ///
    /// It samples on its own thread rather than between batches because a batch
    /// is tens of milliseconds and reading the driver inside the timed region
    /// would put the sensor's latency into the hashrate.
    pub struct Sampler {
        stop: Arc<AtomicBool>,
        readings: Arc<Mutex<Vec<Reading>>>,
        handle: Option<std::thread::JoinHandle<()>>,
        pub source: &'static str,
        pub measures_power: bool,
        /// Whether anything on this machine reports this card's temperature.
        ///
        /// Tracked separately from `measures_power` because the two are missing
        /// on different cards: an NVIDIA part has a temperature and (through
        /// nvidia-smi) a draw, while an Intel part has neither, and a tuner that
        /// conflated them would refuse the wrong rigs.
        pub measures_temperature: bool,
        /// The board power CAP this card is running under, where the tool
        /// reports one. Read once, because it is a setting and not a reading.
        ///
        /// It is here because of what a Tesla T4 measured at repeat 16: 66 to
        /// 67 W against a 70 W limit, SM clock swinging 1140 to 1305 MHz, and
        /// unit_size 64 beating 96 beating 128 - the exact REVERSE of the
        /// ordering on an RX 9070 XT, where the kernel is latency bound and a
        /// bigger batch helps. Both orderings are real and the tuner finds
        /// either one from the hashrate alone. What the cap adds is the reason,
        /// which is the difference between an operator believing the tune and an
        /// operator overriding it.
        pub power_limit_w: Option<f32>,
    }

    #[derive(Clone, Copy, Debug)]
    struct Reading {
        at: Instant,
        temp_c: Option<f32>,
        watts: Option<f32>,
        clock_mhz: Option<f32>,
    }

    #[cfg(windows)]
    fn adl_adapter() -> Option<i32> {
        let reporting = crate::gpu_temp_adl::reporting_gpus();
        // Exactly one card, for the same reason the thermal monitor insists on
        // it: ADL's adapter order is not the OpenCL device order, so with two
        // cards answering there is no honest way to say whose watts these are.
        match reporting.as_slice() {
            [gpu] => Some(gpu.adapter_index),
            _ => None,
        }
    }

    impl Sampler {
        pub fn start(thermal_file: &str, gpu_index: u32, vendor: crate::gpu_arch::GpuVendor) -> Sampler {
            let readings: Arc<Mutex<Vec<Reading>>> = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));

            #[cfg(windows)]
            if thermal_file.trim().is_empty() {
                if let Some(adapter) = adl_adapter() {
                    let sink = Arc::clone(&readings);
                    let flag = Arc::clone(&stop);
                    let handle = std::thread::spawn(move || {
                        while !flag.load(Ordering::Relaxed) {
                            if let Some(sample) = crate::gpu_temp_adl::sample(adapter) {
                                if let Ok(mut out) = sink.lock() {
                                    out.push(Reading {
                                        at: Instant::now(),
                                        temp_c: sample.temp_c,
                                        watts: sample.board_power_w,
                                        clock_mhz: sample.gfx_clock_mhz,
                                    });
                                }
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    });
                    let measures_power = crate::gpu_temp_adl::board_power_w(adapter).is_some();
                    let measures_temperature =
                        crate::gpu_temp_adl::temperature_c(adapter).is_some();
                    return Sampler {
                        stop,
                        readings,
                        handle: Some(handle),
                        source: "AMD driver (ADL) board power, temperature and shader clock at 10 Hz",
                        measures_power,
                        measures_temperature,
                        // ADL reports a board draw but no board power LIMIT
                        // through the path this build uses, so there is nothing
                        // honest to put here on an AMD card.
                        power_limit_w: None,
                    };
                }
            }

            // Everything else: the same backend the running miner publishes from,
            // sampled once a second because each read costs a process spawn.
            match crate::efficiency::detect_gpu_temp_sensor(thermal_file, gpu_index, vendor) {
                Some((backend, _)) => {
                    // One combined read, so the three capabilities are decided
                    // by what the card actually answered rather than by three
                    // separate probes that could disagree. On NVIDIA this is a
                    // single `nvidia-smi` spawn for all three quantities.
                    let first = backend.read_sample();
                    let measures_power = first.watts.is_some();
                    // `detect_gpu_temp_sensor` only returns a backend that has
                    // already answered with a temperature, so this is true; it
                    // is computed rather than assumed so that a future backend
                    // that reports power alone cannot claim a thermometer.
                    let measures_temperature = first.temp_c.is_some();
                    let power_limit_w = backend.read_power_limit_w();
                    let sink = Arc::clone(&readings);
                    let flag = Arc::clone(&stop);
                    let label: &'static str = Box::leak(
                        format!("{} at 1 Hz", backend.label()).into_boxed_str(),
                    );
                    let handle = std::thread::spawn(move || {
                        while !flag.load(Ordering::Relaxed) {
                            let sample = backend.read_sample();
                            if let Ok(mut out) = sink.lock() {
                                out.push(Reading {
                                    at: Instant::now(),
                                    temp_c: sample.temp_c,
                                    watts: sample.watts,
                                    clock_mhz: sample.clock_mhz,
                                });
                            }
                            std::thread::sleep(Duration::from_millis(1000));
                        }
                    });
                    Sampler {
                        stop,
                        readings,
                        handle: Some(handle),
                        source: label,
                        measures_power,
                        measures_temperature,
                        power_limit_w,
                    }
                }
                None => Sampler {
                    stop,
                    readings,
                    handle: None,
                    source: "no GPU sensor on this machine",
                    measures_power: false,
                    measures_temperature: false,
                    power_limit_w: None,
                },
            }
        }

        /// Mean of everything sampled between `from` and now.
        pub fn window(&self, from: Instant) -> TelemetryWindow {
            let Ok(readings) = self.readings.lock() else {
                return TelemetryWindow::default();
            };
            let taken: Vec<&Reading> = readings.iter().filter(|r| r.at >= from).collect();
            let mean = |pick: fn(&Reading) -> Option<f32>| -> Option<f32> {
                let values: Vec<f32> = taken.iter().filter_map(|r| pick(r)).collect();
                (!values.is_empty())
                    .then(|| values.iter().sum::<f32>() / values.len() as f32)
            };
            TelemetryWindow {
                temp_c: mean(|r| r.temp_c),
                peak_temp_c: taken
                    .iter()
                    .filter_map(|r| r.temp_c)
                    .fold(None, |acc: Option<f32>, t| Some(acc.map_or(t, |a| a.max(t)))),
                watts: mean(|r| r.watts),
                clock_mhz: mean(|r| r.clock_mhz),
                samples: taken.len(),
            }
        }

        pub fn peak_temp(&self) -> Option<f32> {
            let readings = self.readings.lock().ok()?;
            readings
                .iter()
                .filter_map(|r| r.temp_c)
                .fold(None, |acc: Option<f32>, t| Some(acc.map_or(t, |a| a.max(t))))
        }

        /// What fraction of this card's power cap a measured draw is, where both
        /// numbers exist.
        pub fn power_cap_load(&self, watts: Option<f32>) -> Option<f64> {
            let limit = f64::from(self.power_limit_w?);
            let watts = f64::from(watts?);
            (limit > 0.0).then_some(watts / limit)
        }
    }

    /// Draw at or above this share of the cap is a card being held BY the cap.
    ///
    /// Measured rather than chosen: a T4 under this kernel reported 66 to 67 W
    /// against a 70 W limit, which is 0.94 to 0.96, while the same card idle
    /// between batches sits near 0.15. Anything in that top band is the limiter
    /// doing the deciding, and it is the one condition under which "give the
    /// card more nonces per launch" is the wrong instinct.
    pub const POWER_CAPPED_FRACTION: f64 = 0.93;

    impl Drop for Sampler {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// One candidate, measured.
    #[derive(Clone, Debug)]
    pub struct Measured {
        pub shape: Shape,
        pub seconds: f64,
        pub nonces: u64,
        pub hashrate: f64,
        pub batch_ms_sorted: Vec<f64>,
        pub telemetry: TelemetryWindow,
        pub cpu_checks: u32,
        /// The minimum hash over each corpus segment, in segment order. Every
        /// candidate hashes the same nonces, so every candidate must produce the
        /// same list, byte for byte.
        pub segment_minimums: Vec<(u32, [u8; 32])>,
    }

    impl Measured {
        pub fn mean_batch_seconds(&self) -> f64 {
            if self.batch_ms_sorted.is_empty() {
                return 0.0;
            }
            self.batch_ms_sorted.iter().sum::<f64>() / self.batch_ms_sorted.len() as f64 / 1000.0
        }
        pub fn p50_ms(&self) -> f64 {
            percentile(&self.batch_ms_sorted, 0.50)
        }
        pub fn p95_ms(&self) -> f64 {
            percentile(&self.batch_ms_sorted, 0.95)
        }
        pub fn score_input(&self, fallback_watts: f64) -> ScoreInput {
            ScoreInput {
                hashrate: self.hashrate,
                mean_batch_seconds: self.mean_batch_seconds(),
                p95_batch_ms: self.p95_ms(),
                gpu_watts: self
                    .telemetry
                    .watts
                    .map(f64::from)
                    .unwrap_or(fallback_watts),
            }
        }
    }

    /// Hash the whole corpus once with `shape`, timing every batch.
    ///
    /// Correctness is checked outside the timed region, deliberately: a CPU
    /// re-hash inside it would be measuring the CPU.
    #[allow(clippy::too_many_arguments)]
    pub fn run_corpus<D: GateDevice>(
        device: &D,
        shape: Shape,
        corpus: &Corpus,
        intros: &[Vec<u8>],
        height: u64,
        sampler: &Sampler,
    ) -> Result<Measured, String> {
        let batches = corpus.batches(shape)?;
        let per_segment = (corpus.segment_nonces / shape.nonces()) as usize;
        let started_at = Instant::now();
        let mut batch_ms = Vec::with_capacity(batches.len());
        let mut results: Vec<(u32, [u8; 32], u32)> = Vec::with_capacity(batches.len());
        let wall_start = Instant::now();
        for batch in &batches {
            let at = Instant::now();
            let (nonce, hash) = device.best(
                shape,
                height,
                &intros[batch.header_index as usize],
                batch.nonce_start,
            )?;
            batch_ms.push(at.elapsed().as_secs_f64() * 1000.0);
            results.push((nonce, hash, batch.header_index));
        }
        let seconds = wall_start.elapsed().as_secs_f64();
        if !seconds.is_finite() || seconds <= 0.0 {
            return Err("non-positive run duration".to_string());
        }

        // Every batch's answer is re-hashed on the CPU with the consensus
        // implementation. A timing number taken from a kernel that returned a
        // wrong hash is worse than no number.
        let mut cpu_checks = 0u32;
        for (index, (nonce, hash, header_index)) in results.iter().enumerate() {
            let batch = &batches[index];
            if nonce.wrapping_sub(batch.nonce_start) as u64 >= batch.nonces {
                return Err(format!(
                    "batch at nonce {} returned nonce {nonce}, outside its own window",
                    batch.nonce_start
                ));
            }
            if cpu_hash(height, &intros[*header_index as usize], *nonce) != *hash {
                return Err(format!(
                    "GPU hash at nonce {nonce} does not match x16rs::block_hash"
                ));
            }
            cpu_checks += 1;
        }

        // The minimum over each segment. Identical work, so this list has to be
        // identical for every candidate; see `agree_on_segments`.
        let mut segment_minimums = Vec::with_capacity(corpus.segments as usize);
        for segment in results.chunks(per_segment.max(1)) {
            let mut best = segment[0];
            for entry in segment {
                if crate::hash_util::hash_more_power(&entry.1, &best.1) {
                    best = *entry;
                }
            }
            segment_minimums.push((best.0, best.1));
        }

        let nonces = corpus.total_nonces();
        batch_ms.sort_by(|a, b| a.total_cmp(b));
        Ok(Measured {
            shape,
            seconds,
            nonces,
            hashrate: nonces as f64 / seconds,
            batch_ms_sorted: batch_ms,
            telemetry: sampler.window(started_at),
            cpu_checks,
            segment_minimums,
        })
    }

    /// Two candidates hashed the same nonces, so they must have found the same
    /// best hash in every segment.
    ///
    /// This is free and it is strong: it is a 100%-coverage comparison of two
    /// launch shapes over tens of millions of nonces, and the reference side of
    /// it has already been proved against the CPU.
    pub fn agree_on_segments(reference: &Measured, other: &Measured) -> Result<(), String> {
        if reference.segment_minimums.len() != other.segment_minimums.len() {
            return Err(format!(
                "shape {}x{} produced {} segment results, the reference produced {}",
                other.shape.work_groups,
                other.shape.unit_size,
                other.segment_minimums.len(),
                reference.segment_minimums.len()
            ));
        }
        for (index, (want, got)) in reference
            .segment_minimums
            .iter()
            .zip(other.segment_minimums.iter())
            .enumerate()
        {
            if want != got {
                return Err(format!(
                    "over corpus segment {index} the reference shape found nonce {} hash {}, \
                     this shape found nonce {} hash {}; the two shapes do not agree on identical work",
                    want.0,
                    hex::encode(want.1),
                    got.0,
                    hex::encode(got.1)
                ));
            }
        }
        Ok(())
    }

    /// What a shape proof covered.
    #[derive(Clone, Copy, Debug)]
    pub struct ShapeProof {
        pub window: u64,
        pub thresholds: usize,
        pub miss_probability: f64,
    }

    /// Prove that ONE launch shape hashes its whole window exactly the way the
    /// CPU consensus implementation does, before its speed is allowed to count.
    ///
    /// The kernel is the same for every shape, and `x16rs_gate equiv` is what
    /// proves the kernel. What changes with the shape is how nonces are placed
    /// on the card and how the per-work-group reduction is built, so what is
    /// proved here is the shape:
    ///
    ///   * with the weakest possible target every nonce qualifies, so the
    ///     kernel's own hit counter must equal the window exactly. A shape that
    ///     drops, repeats or overruns work fails here.
    ///   * at each of several rank thresholds taken from the sorted CPU oracle,
    ///     the kernel's hit count must equal the rank exactly. Each of those
    ///     launches reads every hash in the window.
    ///   * the shares that do come back are compared byte for byte.
    ///   * the best-hash reduction, which is the only path solo mining reads,
    ///     must return the true minimum of the window.
    ///
    /// This is the gate's production-shape pass, re-aimed at a candidate. It is
    /// a separate implementation from `run_equivalence`'s because that one
    /// accumulates a mismatch census for a human to read and aborts the run,
    /// while this one is an admission ticket: a candidate that fails is dropped
    /// and the tune continues.
    pub fn prove_shape<D: GateDevice>(
        device: &D,
        shape: Shape,
        height: u64,
        intro: &[u8],
        nonce_start: u32,
        thresholds: u32,
        oracle_threads: usize,
    ) -> Result<ShapeProof, String> {
        let window = shape.nonces();
        if window == 0 || window > u32::MAX as u64 {
            return Err(format!("launch shape hashes {window} nonces"));
        }
        let cpu = cpu_hash_window(height, intro, nonce_start, window as u32, oracle_threads);
        let mut sorted = cpu.clone();
        sorted.sort_unstable();

        let launch = |target: &[u8; 32]| {
            device.count_and_shares(shape, height, intro, nonce_start, target)
        };

        // 1. The whole window, counted.
        let (all_hits, _) = launch(&[0xffu8; 32])?;
        if all_hits != window {
            return Err(format!(
                "with a target every nonce beats, the kernel counted {all_hits} hits over a \
                 {window}-nonce window; this shape does not hash the work it is given"
            ));
        }

        // 2. Rank thresholds.
        let ranks = threshold_ranks(window, SHARE_LIST_CAPACITY, thresholds);
        for rank in ranks.iter().copied() {
            let target = sorted[(rank - 1) as usize];
            let (hits, shares) = launch(&target)?;
            if hits != rank {
                return Err(format!(
                    "the CPU says exactly {rank} of the {window} hashes are <= {}, the kernel \
                     counted {hits}; this shape's hashes differ from the CPU's",
                    hex::encode(target),
                ));
            }
            for (nonce, hash) in &shares {
                let offset = nonce.wrapping_sub(nonce_start) as u64;
                if offset >= window {
                    return Err(format!("shape returned nonce {nonce}, outside the window"));
                }
                if cpu[offset as usize] != *hash {
                    return Err(format!(
                        "nonce {nonce}: gpu={} cpu={}",
                        hex::encode(hash),
                        hex::encode(cpu[offset as usize])
                    ));
                }
            }
        }

        // 3. The reduction solo mining actually reads.
        let (best_nonce, best_hash) = device.best(shape, height, intro, nonce_start)?;
        if best_hash != sorted[0] {
            return Err(format!(
                "the best-hash reduction returned {} for nonce {best_nonce}; the CPU minimum over \
                 the window is {}",
                hex::encode(best_hash),
                hex::encode(sorted[0])
            ));
        }
        let offset = best_nonce.wrapping_sub(nonce_start) as u64;
        if offset >= window || cpu[offset as usize] != best_hash {
            return Err(format!("best nonce {best_nonce} does not carry its own hash"));
        }

        Ok(ShapeProof {
            window,
            thresholds: ranks.len(),
            miss_probability: threshold_miss_probability(window, &ranks),
        })
    }

    /// Which card, on which backend.
    ///
    /// An enum rather than a set of optional fields so that the two backends
    /// cannot both be half-specified, and so a build that lacks one of them can
    /// still NAME it: a CUDA operator on an OpenCL-only binary gets a sentence
    /// about CUDA, which is the whole point of this variant existing in every
    /// build rather than behind a feature gate.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum TuneTarget {
        OpenCl {
            opencl_dir: String,
            platform: u32,
            device_ids: String,
        },
        Cuda {
            device_index: i32,
        },
    }

    impl TuneTarget {
        pub fn label(&self) -> &'static str {
            match self {
                TuneTarget::OpenCl { .. } => "opencl",
                TuneTarget::Cuda { .. } => "cuda",
            }
        }
    }

    /// Everything the caller has to tell the tuner about the rig.
    #[derive(Clone, Debug)]
    pub struct TuneRequest {
        pub target: TuneTarget,
        pub local_size: u32,
        pub min_work_groups: u32,
        pub max_work_groups: u32,
        pub max_unit_size: u32,
        pub vendor: crate::gpu_arch::GpuVendor,
        pub mode: EfficiencyMode,
        /// Total wall-clock budget for the sweeps, in seconds. The soak runs on
        /// top of it, until the card stops moving.
        ///
        /// It is also what sizes the corpus: `plan_session` divides it over the
        /// passes a sweep will make and caps the corpus segment at that many
        /// seconds of this card's hashing, so raising it buys a finer grid as
        /// well as a longer soak. That is the whole reason the tuner can now
        /// tell an operator which `benchmark_seconds` would measure the shapes
        /// it had to drop.
        pub budget_seconds: u64,
        pub economics: Economics,
        /// Board draw to fall back on when the card has no power sensor.
        pub estimated_watts: f64,
        pub thermal_file: String,
        pub gpu_index: u32,
        pub oracle_threads: usize,
        pub headers: u32,
        pub proof_thresholds: u32,
        /// The operator's own temperature ceiling from `[efficiency] max_temp_c`,
        /// where they set one.
        ///
        /// The running miner honours this by throttling work groups, so a tuner
        /// that ignored it would drive the card past the limit its owner set,
        /// measure a hashrate that limit forbids, and then write that shape into
        /// the config for the miner to be throttled back out of. Measured on the
        /// 9070 XT: the largest shape reaches 90 C, so this is not hypothetical.
        pub max_temp_c: Option<f32>,
    }

    /// What the tuner decided, and everything a reviewer needs to check it.
    pub struct TuneOutcome {
        pub pick: BenchmarkPick,
        pub shape: Shape,
        /// Which backend measured this, and which device it opened. Free text,
        /// straight from the backend, so a report cannot be mistaken for the
        /// other card's.
        pub backend: &'static str,
        pub device: String,
        /// Devices opened over the session. One on OpenCL; one per distinct
        /// (local_size, unit_size, larger work_groups) on CUDA, and printed so
        /// the cost of that is visible rather than inferred.
        pub device_opens: u32,
        /// The finalists' medians and the span each one showed across its own
        /// repeated passes. THIS is the resolution of the choice the tuner made:
        /// see `resolution_note`.
        pub finalists: Vec<FinalistRuns>,
        pub objective: Objective,
        pub watts_source: WattsSource,
        pub soak: Vec<SoakPass>,
        pub soak_seconds: f64,
        pub settle: SettleState,
        pub winner: Measured,
        pub corpus: Corpus,
        pub total_seconds: f64,
        pub final_proof: ShapeProof,
        pub peak_temp_c: Option<f32>,
        /// The card's own board power cap, and what fraction of it the winning
        /// shape drew. Both `None` where nothing reports one.
        pub power_limit_w: Option<f32>,
        pub power_cap_load: Option<f64>,
        /// What the operator's `max_temp_c` was worth on this card. Carried into
        /// the report so a tune that ran without a ceiling says so in the proof
        /// block rather than only in a log line nobody keeps.
        pub ceiling: TempCeiling,
        /// The sensor line, so the report can name what did (or did not) measure.
        pub sensors: &'static str,
        /// What the plan said this would cost, kept so the report can put the
        /// estimate next to the time it really took.
        pub plan: SessionPlan,
    }

    /// Rank thresholds in the proof the winning shape gets after the soak.
    ///
    /// Paid once, so it is sized for the answer rather than for the schedule:
    /// 255 thresholds put a single wrong hash anywhere in the window past every
    /// one of them with probability about 4e-3, against 3e-2 for the 31 an
    /// admission proof uses.
    const FINAL_PROOF_THRESHOLDS: u32 = 255;

    /// How long the probe hashes before its rate is believed, and the most
    /// batches it will spend getting there.
    ///
    /// A third of a second is several times the 100 ms sampler period and long
    /// enough that a launch overhead is not the measurement, while staying short
    /// enough that a card which turns out to be very slow has not already cost
    /// the operator a minute before the tuner can tell them so.
    const PROBE_SECONDS: f64 = 0.35;
    const PROBE_MAX_BATCHES: u32 = 64;

    /// Measure this card, once, on the smallest launch it offers, so the corpus
    /// can be sized from what it really does instead of from what its work-group
    /// ceiling implies.
    ///
    /// The first launch is thrown away: it carries the kernel upload and the
    /// first buffer touch, and counting it would under-report the card by enough
    /// to change the plan. The last answer is re-hashed on the CPU, because a
    /// rate taken from a kernel that returned a wrong hash is not a rate.
    fn probe_rate<D: GateDevice>(
        device: &D,
        shape: Shape,
        height: u64,
        intro: &[u8],
    ) -> Result<f64, String> {
        let batch = shape.nonces();
        if batch == 0 || batch > u32::MAX as u64 {
            return Err(format!("probe shape hashes {batch} nonces"));
        }
        let launch = |nonce_start: u32| device.best(shape, height, intro, nonce_start);
        launch(PROBE_NONCE_BASE)?;

        let started = Instant::now();
        let mut hashed = 0u64;
        let mut last = None;
        for index in 0..PROBE_MAX_BATCHES {
            let nonce_start = PROBE_NONCE_BASE.wrapping_add(
                ((index as u64 + 1) * batch % (1u64 << 32)) as u32,
            );
            last = Some((nonce_start, launch(nonce_start)?));
            hashed += batch;
            if started.elapsed().as_secs_f64() >= PROBE_SECONDS {
                break;
            }
        }
        let seconds = started.elapsed().as_secs_f64();
        if !seconds.is_finite() || seconds <= 0.0 {
            return Err("the probe took no measurable time".to_string());
        }
        if let Some((nonce_start, (nonce, hash))) = last {
            if nonce.wrapping_sub(nonce_start) as u64 >= batch {
                return Err(format!(
                    "the probe returned nonce {nonce}, outside its own {batch}-nonce window"
                ));
            }
            if cpu_hash(height, intro, nonce) != hash {
                return Err(
                    "the probe's hash does not match x16rs::block_hash; this device is not \
                     computing the consensus hash and nothing measured on it would mean anything"
                        .to_string(),
                );
            }
        }
        Ok(hashed as f64 / seconds)
    }

    /// Throwaway batches run after a device is opened, before anything is timed.
    ///
    /// The first launch on a fresh allocation carries the kernel upload (OpenCL)
    /// or the module load and the first touch of several hundred megabytes of
    /// device buffers (CUDA). On the OpenCL path the session opens one device and
    /// the probe absorbs that cost once. On the CUDA path a candidate with a new
    /// `unit_size` needs a new miner, so WITHOUT this every CUDA candidate's
    /// first timed batch would carry it - and not equally: a shape with four
    /// batches per pass would wear a quarter of it and a shape with forty a
    /// fortieth, which is a bias that scales with the axis being tuned. Two
    /// rather than one because the second is what shows the first was enough.
    const WARMUP_BATCHES_AFTER_OPEN: u32 = 2;

    /// Where warm-up batches hash: far from the corpus and far from the probe, so
    /// a warm-up launch can never be confused with measured work.
    const WARMUP_NONCE_BASE: u32 = 0xF000_0000;

    /// The devices a session opens, and the one place the two backends differ.
    ///
    /// OpenCL takes the launch shape per call, so a single allocation sized at
    /// the top corner of the grid serves every candidate under it: that is what
    /// `device_is_bound_to_its_shape() == false` buys, and it is why an OpenCL
    /// tune opens exactly one device and every candidate runs against the same
    /// allocation, none of them flattered by a fresh context.
    ///
    /// CUDA cannot: `cuda_mine_batch` hands the kernel `miner.unit_size`, so a
    /// miner built at 64 asked for 128 would run 64 and report 128. So on that
    /// backend a device is opened per unit_size rather than per CANDIDATE. Work
    /// groups are NOT baked in - `mine_block_batch_shares` clamps them per launch
    /// against buffers sized at construction - so once the session knows which
    /// shapes it will measure, one miner allocated at the largest work-group
    /// count for a given unit_size serves every candidate that shares it. On a
    /// typical grid that is a third of them, and it is the difference between
    /// three device opens and twenty-four.
    struct Devices<'b, B: GateBackend> {
        backend: &'b B,
        /// The allocation shared by every candidate, where the backend allows a
        /// shared one. Ignored entirely when it does not.
        allocation: Shape,
        bound: bool,
        /// Shapes this session may ask for. Used only to size a bound backend's
        /// allocations, and empty until the plan exists: the probe runs before
        /// there is a plan, and it gets an allocation of its own exact shape.
        plan: Vec<Shape>,
        open: Option<(Shape, B::Device)>,
        height: u64,
        warm_intro: Vec<u8>,
        opens: u32,
    }

    impl<'b, B: GateBackend> Devices<'b, B> {
        fn new(backend: &'b B, allocation: Shape, height: u64, warm_intro: Vec<u8>) -> Self {
            Devices {
                backend,
                allocation,
                bound: backend.device_is_bound_to_its_shape(),
                plan: Vec::new(),
                open: None,
                height,
                warm_intro,
                opens: 0,
            }
        }

        /// Tell the pool which shapes the session will ask for, so a bound
        /// backend can allocate once per unit_size instead of once per shape.
        fn will_measure(&mut self, shapes: &[Shape]) {
            self.plan = shapes.to_vec();
        }

        /// The shape a device must be ALLOCATED at to be able to launch `shape`.
        ///
        /// On an unbound backend that is the grid ceiling, always, so the whole
        /// session runs against one allocation. On a bound one it is `shape`'s
        /// own unit_size, which there is no choice about, with the work-group
        /// count [`shared_allocation_work_groups`] derives from the plan.
        fn allocation_for(&self, shape: Shape) -> Shape {
            if !self.bound {
                return self.allocation;
            }
            Shape {
                work_groups: shared_allocation_work_groups(&self.plan, shape),
                ..shape
            }
        }

        /// A device that can really launch `shape`, opening one if the currently
        /// open device cannot.
        fn at(&mut self, shape: Shape) -> Result<&B::Device, String> {
            let serves = self
                .open
                .as_ref()
                .is_some_and(|(_, device)| device.can_launch(shape));
            if !serves {
                let allocate_at = self.allocation_for(shape);
                self.backend.check_shape(allocate_at)?;
                // Dropped BEFORE the new one is asked for. On CUDA the old
                // miner holds hundreds of megabytes of device memory and the
                // new one needs its own; holding both would make the largest
                // candidate fail to allocate on exactly the cards where it
                // matters most.
                self.open = None;
                let device = self.backend.open(allocate_at)?;
                if !device.can_launch(shape) {
                    return Err(format!(
                        "{} opened a device for {allocate_at} that cannot launch {shape}",
                        self.backend.name()
                    ));
                }
                warm_up(&device, shape, self.height, &self.warm_intro)?;
                self.opens += 1;
                self.open = Some((allocate_at, device));
            }
            Ok(&self
                .open
                .as_ref()
                .expect("a device was just opened or already served this shape")
                .1)
        }
    }

    /// Untimed launches that leave the card in the state a measurement expects.
    fn warm_up<D: GateDevice>(
        device: &D,
        shape: Shape,
        height: u64,
        intro: &[u8],
    ) -> Result<(), String> {
        let batch = shape.nonces();
        for index in 0..WARMUP_BATCHES_AFTER_OPEN {
            let nonce_start =
                WARMUP_NONCE_BASE.wrapping_add(((index as u64 * batch) % (1u64 << 32)) as u32);
            device
                .best(shape, height, intro, nonce_start)
                .map_err(|error| format!("warm-up batch {} at {shape}: {error}", index + 1))?;
        }
        Ok(())
    }

    /// Run the tune. Returns the shape to write, or the reason there is none.
    ///
    /// The dispatch is the only place either backend is named. Everything after
    /// it is [`tune_on`], compiled once and identical for both.
    pub fn tune(request: &TuneRequest) -> Result<TuneOutcome, String> {
        match &request.target {
            TuneTarget::OpenCl {
                opencl_dir,
                platform,
                device_ids,
            } => {
                #[cfg(feature = "ocl")]
                {
                    tune_on(
                        &crate::x16rs_gate::OclBackend {
                            opencl_dir: opencl_dir.clone(),
                            platform: *platform,
                            device: device_ids.clone(),
                        },
                        request,
                    )
                }
                #[cfg(not(feature = "ocl"))]
                {
                    let _ = (opencl_dir, platform, device_ids);
                    Err("this binary was built without the OpenCL backend, so it cannot tune an \
                         OpenCL device. Rebuild with --features ocl. Config unchanged"
                        .to_string())
                }
            }
            TuneTarget::Cuda { device_index } => {
                #[cfg(feature = "cuda")]
                {
                    // Two different failures with two different remedies, and
                    // the reason this check is here rather than at the call
                    // site: `cuda` is a cargo feature that only adds the crate,
                    // while whether that crate holds KERNELS is decided by its
                    // build script finding nvcc. A binary can have the feature
                    // and no kernels, and every device call then returns
                    // NotCompiled, which used to reach the operator as a driver
                    // error.
                    if !crate::x16rs_gate::cuda_kernels_available() {
                        return Err(
                            "this binary has the cuda feature but NO CUDA kernels: \
                             x16rs-cuda/build.rs did not find nvcc when it was built, so \
                             cfg(cuda_available) was never set and every device call returns \
                             `x16rs-cuda built without CUDA kernels`. Install the CUDA Toolkit, \
                             set CUDA_PATH, and rebuild with --features cuda; the build prints \
                             `Using CUDA Toolkit at ...` when it found one. Config unchanged"
                                .to_string(),
                        );
                    }
                    tune_on(
                        &crate::x16rs_gate::CudaBackend {
                            device_index: *device_index,
                        },
                        request,
                    )
                }
                #[cfg(not(feature = "cuda"))]
                {
                    let _ = device_index;
                    Err("this binary was built without the CUDA backend, so it cannot tune an \
                         NVIDIA card. Rebuild with --features cuda (the CUDA Toolkit must be \
                         installed and CUDA_PATH set, or the build silently produces a binary \
                         with no kernels). Config unchanged"
                        .to_string())
                }
            }
        }
    }

    /// The tune itself, on whichever backend was handed in.
    pub fn tune_on<B: GateBackend>(
        backend: &B,
        request: &TuneRequest,
    ) -> Result<TuneOutcome, String> {
        let started = Instant::now();
        let height = REPEAT16_HEIGHT;
        let repeat = x16rs::block_hash_repeat(height);
        let local_size = request.local_size;

        // The whole universe is opened for, because the device allocation has to
        // be the same for every candidate; which of those shapes is worth
        // measuring is decided after the probe, not before it.
        let universe = candidate_universe(
            request.min_work_groups,
            request.max_work_groups,
            request.max_unit_size,
            local_size,
        );
        if universe.is_empty() {
            return Err("no launch shape fits this device's limits".to_string());
        }

        // The exhaustive proof each candidate has to pass reads the kernel's own
        // share list, so a backend whose list is a different size would make
        // every "over its whole window" claim mean something else. This is the
        // same refusal `run_equivalence_on` makes, for the same reason, before
        // anything is measured.
        let capacity = backend.share_capacity();
        if capacity != SHARE_LIST_CAPACITY {
            return Err(format!(
                "{} reports a share list capacity of {capacity}, this tuner proves shapes against \
                 {SHARE_LIST_CAPACITY}",
                backend.name()
            ));
        }
        wlogln!(
            "[autotune] backend {} on {}",
            backend.name(),
            backend.describe()
        );

        let sampler = Sampler::start(&request.thermal_file, request.gpu_index, request.vendor);
        let watts_source = if sampler.measures_power {
            WattsSource::Measured
        } else {
            WattsSource::Estimated
        };
        let (objective, fallback_reason) = resolve_objective(request.mode, &request.economics);
        wlogln!("[autotune] sensors: {}", sampler.source);
        wlogln!(
            "[autotune] mode={} optimises {} on {} watts",
            request.mode.label(),
            objective.label(),
            watts_source.label()
        );
        if let Some(reason) = fallback_reason {
            wlogln!("[autotune] NOTE: {reason}");
        }
        // Defect 1, said where it bites rather than left to be inferred from one
        // word in the line above. With no per-candidate watt figure, every shape
        // is divided by the same constant, so hashes-per-joule and net-EUR are
        // affine in the hashrate and rank exactly as throughput does. An
        // operator who chose Eco is being given Max, and has a right to know it
        // before spending the tune.
        if watts_source == WattsSource::Estimated && objective != Objective::ValidHashrate {
            wlogln!(
                "[autotune] NOTE: nothing on this machine reports this card's power draw, so every \
                 candidate is scored on the same estimated {:.0} W. That makes {} rank the shapes \
                 in exactly the order {} would: this tune cannot tell {} apart from max. Only a \
                 card that reports its own watts can, and no [efficiency] gpu_watts value changes \
                 it, because one constant divides every candidate alike",
                request.estimated_watts,
                objective.label(),
                Objective::ValidHashrate.label(),
                request.mode.label(),
            );
        }

        // The other silent no-op: a temperature ceiling on a card with no
        // thermometer. `within_temperature_limit` compared nothing and returned
        // a pass, so "refuses a candidate past max_temp_c" did nothing at all.
        // Refused here, before the sweep, because the running miner fails closed
        // on a missing sensor and a tuner that pushed the card anyway would
        // choose a shape the miner will then refuse to run.
        let ceiling_state = TempCeiling::resolve(request.max_temp_c, sampler.measures_temperature);
        wlogln!(
            "[autotune] temperature: {}",
            ceiling_state.describe(sampler.source)
        );
        if let TempCeiling::Unenforceable { limit_c } = ceiling_state {
            return Err(format!(
                "[efficiency] max_temp_c is {limit_c:.0} C but nothing on this machine reports \
                 this GPU's temperature ({}), so the ceiling cannot be enforced and a tune would \
                 be free to pick the hottest shape on the card. Either set max_temp_c = 0 to say \
                 you are not asking for one, or install a sensor this build can read: rocm-smi or \
                 amd-smi for AMD, nvidia-smi for NVIDIA, or point [efficiency] thermal_file at a \
                 hwmon temperature file. Config unchanged",
                sampler.source
            ));
        }

        // On a backend that allows it the device is opened once, at the top
        // corner of the whole grid, so every candidate runs against the same
        // allocation and none of them is flattered by a fresh context. It is the
        // whole grid rather than the planned subset on purpose: the plan is not
        // known until the probe has run on this device, and an allocation that
        // changed with the probe's answer would make the sweep depend on it
        // twice. On a backend where a shape is baked into the allocation this
        // ceiling is unused and [`Devices`] opens per shape instead. Either way
        // the winner is re-opened at its own exact shape for the soak, which is
        // what it will mine with.
        let ceiling = Shape {
            work_groups: universe.iter().map(|s| s.work_groups).max().unwrap_or(1),
            local_size,
            unit_size: universe.iter().map(|s| s.unit_size).max().unwrap_or(32),
        };
        let probe_intro = corpus_header(0);
        let mut devices = Devices::new(backend, ceiling, height, probe_intro.clone());

        // Probe first, plan second. The corpus is sized from what this card
        // really does, so a pass is a known number of seconds before anything is
        // committed to, rather than a number of batches that happened to be
        // affordable on the card the tuner was written on.
        let probe_shape = universe
            .iter()
            .copied()
            .min_by_key(|shape| shape.nonces())
            .unwrap_or(universe[0]);
        let probe_hps = {
            let device = devices.at(probe_shape)?;
            probe_rate(device, probe_shape, height, &probe_intro)?
        };
        let plan = plan_session(
            request.min_work_groups,
            request.max_work_groups,
            request.max_unit_size,
            local_size,
            probe_hps,
            request.budget_seconds,
            request.headers,
            NONCE_BASE,
        )?;
        let corpus = plan.corpus;
        let usable = plan.usable.clone();
        let candidates = plan.candidates.clone();
        // Every shape this session can still ask for, coarse points and their
        // refinement neighbours alike. A backend that bakes the shape into the
        // allocation uses it to open once per unit_size instead of once per
        // candidate; an unbound one ignores it.
        devices.will_measure(&usable);

        wlogln!(
            "[autotune] probe {}x{}x{}: {} -> a pass may last {:.1}s (sweep budget {}s, soak needs \
             passes under {:.1}s)",
            probe_shape.work_groups,
            probe_shape.local_size,
            probe_shape.unit_size,
            crate::bench_mainnet_repeat16::fmt_rate(probe_hps),
            plan.pass_ceiling_seconds,
            request.budget_seconds,
            max_soak_pass_seconds(request.budget_seconds),
        );
        // Named individually up to a point, then counted. On an RX 9070 XT the
        // whole grid is fifteen shapes and every drop fits on screen; a card
        // whose work-group ceiling is measured from its VRAM has a grid of
        // seventy-five, and a wall of near-identical lines is how an operator
        // learns to scroll past the ones that matter.
        const NAMED_DROPS: usize = 8;
        for shape in plan.over_ceiling.iter().take(NAMED_DROPS) {
            wlogln!(
                "[autotune] not measured: {}x{}x{} would take about {:.0} ms per batch, over the \
                 {:.0} ms ceiling, so it would be refused however fast it hashed",
                shape.work_groups,
                shape.local_size,
                shape.unit_size,
                shape.nonces() as f64 / probe_hps * 1000.0,
                P95_BATCH_CEILING_MS,
            );
        }
        if plan.over_ceiling.len() > NAMED_DROPS {
            wlogln!(
                "[autotune] and {} more shape(s) over the {:.0} ms batch ceiling, all larger than \
                 the ones above",
                plan.over_ceiling.len() - NAMED_DROPS,
                P95_BATCH_CEILING_MS,
            );
        }
        for shape in plan.off_corpus.iter().take(NAMED_DROPS) {
            wlogln!(
                "[autotune] not measured: {}x{}x{} ({} nonces per batch) would push the shared \
                 corpus segment past the {:.1}s a pass may take",
                shape.work_groups,
                shape.local_size,
                shape.unit_size,
                shape.nonces(),
                plan.pass_ceiling_seconds,
            );
        }
        if plan.off_corpus.len() > NAMED_DROPS {
            wlogln!(
                "[autotune] and {} more shape(s) the shared corpus cannot afford",
                plan.off_corpus.len() - NAMED_DROPS
            );
        }
        if !plan.off_corpus.is_empty() {
            match plan.budget_for_every_shape {
                Some(seconds) => wlogln!(
                    "[autotune] {} shape(s) were dropped for cost, not for correctness. To measure \
                     all of them set [efficiency] benchmark_seconds = {seconds}",
                    plan.off_corpus.len()
                ),
                None => wlogln!(
                    "[autotune] {} shape(s) were dropped for cost. No benchmark_seconds buys them \
                     back: sharing a corpus with them would need a pass longer than any soak can \
                     settle on. Lower [gpu] work_groups to measure them",
                    plan.off_corpus.len()
                ),
            }
        }
        // The other cost that scales with the launch shape, and the one nobody
        // was told about: every candidate's admission proof CPU-hashes its whole
        // batch window. Say what that is going to cost before spending it.
        let (proof_seconds, proof_bytes) = candidates
            .iter()
            .map(|shape| crate::x16rs_gate::oracle_cost(shape.nonces(), request.oracle_threads))
            .fold((0.0f64, 0u64), |(seconds, bytes), (s, b)| {
                (seconds + s, bytes.max(b))
            });
        wlogln!(
            "[autotune] x16rs repeat={repeat} (height {height}), local_size={local_size}, \
             {} candidate shapes of {} in the grid, corpus {} segments x {} nonces = {} nonces \
             (~{:.1}s a pass, ~{:.0}s of hashing for the sweeps)",
            candidates.len(),
            universe.len(),
            corpus.segments,
            corpus.segment_nonces,
            corpus.total_nonces(),
            plan.pass_seconds,
            plan.sweep_seconds,
        );
        wlogln!(
            "[autotune] on top of that the CPU oracle proves every candidate over its whole batch \
             window: about {:.0}s on {} threads, peaking at {} MB. This is what buys the \
             consensus guarantee, and it is why shapes over the batch ceiling are not measured",
            proof_seconds,
            request.oracle_threads,
            proof_bytes / (1024 * 1024),
        );
        wlogln!(
            "[autotune] estimated total before the soak: about {:.0}s",
            plan.sweep_seconds + proof_seconds
        );

        let intros: Vec<Vec<u8>> = (0..corpus.headers).map(corpus_header).collect();

        // Coarse sweep.
        let mut reference: Option<Measured> = None;
        let mut results: Vec<Measured> = Vec::new();
        for shape in &candidates {
            match devices.at(*shape).and_then(|device| {
                measure_candidate(
                    device,
                    *shape,
                    &corpus,
                    &intros,
                    height,
                    &sampler,
                    request,
                    reference.as_ref(),
                )
            }) {
                Ok(measured) => {
                    report_candidate(&measured, objective, request, watts_source, "");
                    if reference.is_none() {
                        reference = Some(measured.clone());
                    }
                    results.push(measured);
                }
                Err(error) => wlogln!(
                    "[autotune] {}x{}: REJECTED ({error})",
                    shape.work_groups,
                    shape.unit_size
                ),
            }
        }
        let fallback_watts = request.estimated_watts;
        let admissible = |m: &Measured| {
            score(
                &m.score_input(fallback_watts),
                objective,
                &request.economics,
                P95_BATCH_CEILING_MS,
            )
        };
        let mut ranked: Vec<(&Measured, f64)> = results
            .iter()
            .filter_map(|m| admissible(m).map(|s| (m, s)))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        if ranked.is_empty() {
            return Err(
                "no candidate produced an admissible measurement (check the latency ceiling \
                 and the rejections above)"
                    .to_string(),
            );
        }

        // Refinement: the grid points the coarse sweep skipped next to the two
        // best candidates, then the finalists re-run in alternating order. Two
        // rather than one because the coarse grid is half the full grid, so the
        // real optimum can sit between the top two.
        let mut neighbours: Vec<Shape> = Vec::new();
        for (measured, _) in ranked.iter().take(2) {
            for shape in refine_candidates(
                measured.shape,
                request.min_work_groups,
                request.max_work_groups,
                request.max_unit_size,
            ) {
                if usable.contains(&shape)
                    && !candidates.contains(&shape)
                    && !neighbours.contains(&shape)
                {
                    neighbours.push(shape);
                }
            }
        }
        if neighbours.is_empty() {
            // Not a silent skip. When the budget bought only the 2x grid, every
            // point next to the winner is a 1.5x point that is not in the
            // corpus, and the sweep the operator got is the whole search.
            wlogln!(
                "[autotune] no refinement points: every grid point next to the leaders is already \
                 measured or was not in the corpus{}",
                match plan.budget_for_every_shape {
                    Some(seconds) if !plan.off_corpus.is_empty() =>
                        format!(", which benchmark_seconds = {seconds} would change"),
                    _ => String::new(),
                }
            );
        }
        for shape in &neighbours {
            match devices.at(*shape).and_then(|device| {
                measure_candidate(
                    device,
                    *shape,
                    &corpus,
                    &intros,
                    height,
                    &sampler,
                    request,
                    reference.as_ref(),
                )
            }) {
                Ok(measured) => {
                    report_candidate(&measured, objective, request, watts_source, " (refine)");
                    results.push(measured);
                }
                Err(error) => wlogln!(
                    "[autotune] refine {}x{}: REJECTED ({error})",
                    shape.work_groups,
                    shape.unit_size
                ),
            }
        }

        let mut ranked: Vec<(Measured, f64)> = results
            .iter()
            .filter_map(|m| admissible(m).map(|s| (m.clone(), s)))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let finalists: Vec<Shape> = ranked
            .iter()
            .take(3)
            .map(|(m, _)| m.shape)
            .collect();

        let mut finalist_runs: Vec<FinalistRuns> = finalists
            .iter()
            .map(|shape| FinalistRuns {
                shape: *shape,
                scores: Vec::new(),
            })
            .collect();
        let winner = if finalists.len() > 1 {
            let rounds = 3;
            wlogln!(
                "[autotune] final round: {} finalists x {rounds} passes, order alternated so the \
                 last one is not flattered by a hotter card. Each shape's own spread across its \
                 repeats is what this comparison can resolve, and it is reported",
                finalists.len()
            );
            for round in 0..rounds {
                let mut order: Vec<usize> = (0..finalist_runs.len()).collect();
                if round % 2 == 1 {
                    order.reverse();
                }
                for index in order {
                    let shape = finalist_runs[index].shape;
                    let measured = devices
                        .at(shape)
                        .and_then(|device| {
                            run_corpus(device, shape, &corpus, &intros, height, &sampler)
                        });
                    match measured {
                        Ok(measured) => {
                            if let Some(reference) = reference.as_ref() {
                                if let Err(error) = agree_on_segments(reference, &measured) {
                                    return Err(format!(
                                        "final round, shape {}x{}: {error}",
                                        shape.work_groups, shape.unit_size
                                    ));
                                }
                            }
                            let input = measured.score_input(fallback_watts);
                            if let Some(value) =
                                score(&input, objective, &request.economics, P95_BATCH_CEILING_MS)
                            {
                                finalist_runs[index].scores.push(value);
                            }
                        }
                        Err(error) => wlogln!(
                            "[autotune] final round {}x{}: {error}",
                            shape.work_groups,
                            shape.unit_size
                        ),
                    }
                }
            }
            for runs in &finalist_runs {
                wlogln!(
                    "[autotune] final {}x{}x{}: median {} = {:.4} over {} passes, own spread \
                     {:.2}%",
                    runs.shape.work_groups,
                    local_size,
                    runs.shape.unit_size,
                    objective.label(),
                    runs.median(),
                    runs.scores.len(),
                    runs.span_pct(),
                );
            }
            let resolution = comparison_resolution_pct(&finalist_runs);
            match winning_margin_pct(&finalist_runs) {
                Some(margin) if margin < resolution => wlogln!(
                    "[autotune] the top two finalists are {margin:.2}% apart and a single shape's \
                     own repeats spanned {resolution:.2}%, so this round did NOT tell them apart. \
                     The winner below is the larger median, and it is a coin toss between two \
                     shapes that measure the same"
                ),
                Some(margin) => wlogln!(
                    "[autotune] the winner beat the runner-up by {margin:.2}% against a \
                     {resolution:.2}% measurement spread, so the choice is resolved"
                ),
                None => {}
            }
            finalist_runs
                .iter()
                .filter(|runs| !runs.scores.is_empty())
                .max_by(|a, b| a.median().total_cmp(&b.median()))
                .map(|runs| runs.shape)
                .unwrap_or(finalists[0])
        } else {
            finalists[0]
        };
        let device_opens_in_sweep = devices.opens;
        // Every allocation the sweep made is released before the soak asks for
        // its own. On CUDA the sweep's miner and the soak's would otherwise both
        // hold their global_hashes buffers, and the winner is usually one of the
        // largest shapes measured.
        drop(devices);

        // Soak, at the winner's own allocation: the buffers the miner will
        // really run with, on both backends.
        backend.check_shape(winner)?;
        let soak_device = backend.open(winner)?;
        warm_up(&soak_device, winner, height, &intros[0])?;
        let (soak, settle, soak_seconds, final_measure) = soak_until_settled(
            &soak_device,
            winner,
            &corpus,
            &intros,
            height,
            &sampler,
            request.budget_seconds,
            request.max_temp_c,
        )?;
        if let Some(reference) = reference.as_ref() {
            agree_on_segments(reference, &final_measure)
                .map_err(|error| format!("after the soak: {error}"))?;
        }

        // The winner is the only shape that gets written into a config and mined
        // with, so it is proved again at full strength, at its own allocation,
        // after the soak. The admission proof each candidate passed is sized to
        // be affordable eleven times over; this one is sized to be conclusive,
        // and its cost is paid once.
        let final_proof = prove_shape(
            &soak_device,
            winner,
            height,
            &intros[0],
            corpus.nonce_start,
            FINAL_PROOF_THRESHOLDS,
            request.oracle_threads,
        )
        .map_err(|error| {
            format!("the chosen shape failed its full-strength equivalence proof: {error}")
        })?;
        wlogln!(
            "[autotune] chosen shape re-proved at full strength: {} thresholds over its whole \
             {}-nonce window, one wrong hash escapes with p = {:.1e}",
            final_proof.thresholds,
            final_proof.window,
            final_proof.miss_probability
        );
        drop(soak_device);

        let final_measure_watts = final_measure.telemetry.watts;
        let pick = pick_for_shape(
            request.vendor,
            winner,
            request.max_work_groups,
            request.max_unit_size,
        );
        Ok(TuneOutcome {
            pick,
            shape: winner,
            backend: backend.name(),
            device: backend.describe(),
            // The soak's own device is counted too: it is a real allocation and
            // on CUDA it is a whole extra miner.
            device_opens: device_opens_in_sweep + 1,
            finalists: finalist_runs,
            objective,
            watts_source,
            soak,
            soak_seconds,
            settle,
            winner: final_measure,
            corpus,
            total_seconds: started.elapsed().as_secs_f64(),
            final_proof,
            peak_temp_c: sampler.peak_temp(),
            power_limit_w: sampler.power_limit_w,
            // Measured on the WINNER's own soak window, not on the session, so
            // it describes the shape the config is about to be given.
            power_cap_load: sampler.power_cap_load(final_measure_watts),
            ceiling: ceiling_state,
            sensors: sampler.source,
            plan,
        })
    }

    /// Prove a shape, then measure it, then check it agrees with the reference.
    #[allow(clippy::too_many_arguments)]
    fn measure_candidate<D: GateDevice>(
        device: &D,
        shape: Shape,
        corpus: &Corpus,
        intros: &[Vec<u8>],
        height: u64,
        sampler: &Sampler,
        request: &TuneRequest,
        reference: Option<&Measured>,
    ) -> Result<Measured, String> {
        let proof = prove_shape(
            device,
            shape,
            height,
            &intros[0],
            corpus.nonce_start,
            request.proof_thresholds,
            request.oracle_threads,
        )
        .map_err(|error| format!("failed the equivalence proof: {error}"))?;
        wlogln!(
            "[autotune] {}x{}x{} proved against the CPU over its whole {}-nonce window: {} count \
             thresholds, one wrong hash slips past them all with p = {:.1e}",
            shape.work_groups,
            shape.local_size,
            shape.unit_size,
            proof.window,
            proof.thresholds,
            proof.miss_probability
        );
        let measured = run_corpus(device, shape, corpus, intros, height, sampler)?;
        if let TempWindow::NotMeasured { limit_c } =
            within_temperature_limit(&measured.telemetry, request.max_temp_c)?
        {
            // The session guard already proved this card has a thermometer, so
            // this is a short window the sampler did not land in, not a missing
            // sensor. Said anyway: a candidate admitted without a temperature
            // check is not a candidate proved to stay under the ceiling.
            wlogln!(
                "[autotune] {}x{}x{}: no temperature sample landed in this window, so the {:.0} C \
                 ceiling was not checked for it",
                shape.work_groups,
                shape.local_size,
                shape.unit_size,
                limit_c,
            );
        }
        if let Some(reference) = reference {
            agree_on_segments(reference, &measured)?;
        }
        Ok(measured)
    }

    /// A shape that runs the card past the operator's ceiling is not a candidate.
    ///
    /// The `Ok` is a state, not a pass: `TempWindow::NotMeasured` says the
    /// window carried no temperature and therefore nothing was checked. Callers
    /// must not read it as a shape that stayed cool.
    pub fn within_temperature_limit(
        telemetry: &TelemetryWindow,
        max_temp_c: Option<f32>,
    ) -> Result<TempWindow, String> {
        temp_window_state(telemetry.peak_temp_c, max_temp_c)
    }

    fn report_candidate(
        measured: &Measured,
        objective: Objective,
        request: &TuneRequest,
        watts_source: WattsSource,
        suffix: &str,
    ) {
        let input = measured.score_input(request.estimated_watts);
        let value = score(
            &input,
            objective,
            &request.economics,
            P95_BATCH_CEILING_MS,
        );
        wlogln!(
            "[autotune] {}x{}x{}{}: {} | p50 {:.0}ms p95 {:.0}ms | {} {:.0}W {}C | {} = {}",
            measured.shape.work_groups,
            measured.shape.local_size,
            measured.shape.unit_size,
            suffix,
            crate::bench_mainnet_repeat16::fmt_rate(measured.hashrate),
            measured.p50_ms(),
            measured.p95_ms(),
            watts_source.label(),
            input.gpu_watts,
            measured
                .telemetry
                .temp_c
                .map(|t| format!("{t:.0}"))
                .unwrap_or_else(|| "?".to_string()),
            objective.label(),
            match value {
                Some(value) => format!("{value:.4}"),
                None => format!(
                    "REFUSED (p95 {:.0}ms over the {:.0}ms ceiling)",
                    measured.p95_ms(),
                    P95_BATCH_CEILING_MS
                ),
            }
        );
    }

    /// Hash the corpus over and over until temperature, board power, shader
    /// clock and hashrate have all stopped moving, or the cap is reached.
    #[allow(clippy::too_many_arguments)]
    fn soak_until_settled<D: GateDevice>(
        device: &D,
        shape: Shape,
        corpus: &Corpus,
        intros: &[Vec<u8>],
        height: u64,
        sampler: &Sampler,
        budget_seconds: u64,
        max_temp_c: Option<f32>,
    ) -> Result<(Vec<SoakPass>, SettleState, f64, Measured), String> {
        let limits = SettleLimits::default();
        // A soak has to be long enough for an air-cooled card to reach its
        // steady temperature, which is minutes. The cap is the larger of a
        // fixed floor and half the operator's budget so that a long budget
        // buys a longer soak and a short one still gets a real soak. The floor
        // exists because the sweep leaves the card already hot: without one, a
        // shape can be declared settled on five passes taken over fifteen
        // seconds, which shows that nothing changed in fifteen seconds and not
        // that the shape sustains.
        //
        // Both come from `soak_cap_seconds` / `soak_floor_seconds` rather than
        // from numbers written out here, because `plan_session` sized the corpus
        // against exactly those two and the two must not be able to drift apart.
        let cap = Duration::from_secs_f64(soak_cap_seconds(budget_seconds));
        let floor = Duration::from_secs_f64(soak_floor_seconds(budget_seconds));
        let started = Instant::now();
        let mut passes: Vec<SoakPass> = Vec::new();
        let mut last: Option<Measured> = None;
        let mut state = SettleState::default();
        let mut unchecked_passes = 0usize;
        wlogln!(
            "[autotune] soak at {}x{}x{}: running until temperature, power, clock and hashrate \
             are all flat, for at least {:.0}s and at most {:.0}s; a pass has to stay under \
             {:.1}s for {} of them to fit",
            shape.work_groups,
            shape.local_size,
            shape.unit_size,
            floor.as_secs_f64(),
            cap.as_secs_f64(),
            max_soak_pass_seconds(budget_seconds),
            soak_window_passes(),
        );
        while started.elapsed() < cap {
            let at = Instant::now();
            let measured = run_corpus(device, shape, corpus, intros, height, sampler)?;
            let telemetry = sampler.window(at);
            // A shape can pass a two-second sweep and then climb past the
            // ceiling once it has been running for a minute. That is exactly
            // what a soak is for, so the limit is enforced here too.
            let checked = within_temperature_limit(&telemetry, max_temp_c)
                .map_err(|error| format!("during the soak the chosen shape {error}"))?;
            if matches!(checked, TempWindow::NotMeasured { .. }) {
                unchecked_passes += 1;
            }
            passes.push(SoakPass {
                seconds: measured.seconds,
                hashrate: measured.hashrate,
                p95_ms: measured.p95_ms(),
                temp_c: telemetry.temp_c,
                watts: telemetry.watts,
                clock_mhz: telemetry.clock_mhz,
            });
            last = Some(measured);
            state = settle_state(&passes, &limits);
            if state.settled && started.elapsed() >= floor {
                break;
            }
        }
        let elapsed = started.elapsed().as_secs_f64();
        let measured = last.ok_or_else(|| "the soak completed no passes".to_string())?;
        let absent = state.absent_signals();
        if !absent.is_empty() {
            wlogln!(
                "[autotune] the soak judged flatness on the hashrate alone where it had to: this \
                 card reports no {}. \"Settled\" here is a weaker claim than on a card that \
                 reports all four",
                absent.join(", no ")
            );
        }
        if unchecked_passes > 0 {
            wlogln!(
                "[autotune] {unchecked_passes} of the {} soak passes carried no temperature \
                 sample, so the ceiling was not checked over them",
                passes.len()
            );
        }
        if !state.settled {
            // Say which of the two it was, because the remedies are opposite. A
            // soak that ran out of passes needs a shorter corpus (which is the
            // planner's job and should never happen now); a soak that made its
            // passes and stayed noisy needs more time.
            if passes.len() < soak_window_passes() {
                wlogln!(
                    "[autotune] the soak fitted only {} of the {} passes it needs inside {:.0}s: \
                     a pass took {:.1}s against the {:.1}s the plan plans for. This is the corpus \
                     being too long for the budget, not the card being unstable",
                    passes.len(),
                    soak_window_passes(),
                    cap.as_secs_f64(),
                    passes.last().map(|p| p.seconds).unwrap_or(0.0),
                    max_soak_pass_seconds(budget_seconds),
                );
            } else {
                wlogln!(
                    "[autotune] the soak made {} passes in {elapsed:.0}s and the card was still \
                     moving: hashrate span {:.2}%. This one really is answered by a larger \
                     benchmark_seconds",
                    passes.len(),
                    state.rate_span_pct,
                );
            }
        }
        Ok((passes, state, elapsed, measured))
    }

    /// The human-readable proof block.
    pub fn render(outcome: &TuneOutcome, request: &TuneRequest) -> String {
        let input = outcome.winner.score_input(request.estimated_watts);
        let mut text = format!(
            "  backend / device : {} / {}\n  \
             devices opened   : {} ({})\n  \
             workload         : x16rs repeat = {} (height {}), the same rounds the live chain runs\n  \
             corpus           : {} segments x {} nonces = {} nonces, headers {}, nonce base {}\n                     \
             every candidate hashed exactly these nonces against exactly these headers\n  \
             chosen shape     : work_groups={} local_size={} unit_size={} ({} nonces per batch)\n  \
             profile written  : {}\n  \
             objective        : {} ({} mode) on {} watts\n  \
             sustained        : {} raw, {} after the {:.2}% a template change throws away\n  \
             batch latency    : p50 {:.0} ms, p95 {:.0} ms (ceiling {:.0} ms)\n  \
             board power      : {}\n  \
             temperature      : {} while running, {} at the end of the soak\n  \
             ceiling          : {}\n  \
             equivalence      : {} count thresholds over the whole {}-nonce launch window; one \
wrong hash escapes with p = {:.1e}\n  \
             CPU verification : {} batches re-hashed with x16rs::block_hash, byte-equal\n",
            outcome.backend,
            outcome.device,
            outcome.device_opens,
            if outcome.device_opens > 2 {
                "this backend bakes the launch shape into the allocation, so a candidate with a \
                 new unit_size needs its own device; each one is warmed up before it is timed"
            } else {
                "one allocation served the sweep, plus the winner's own for the soak"
            },
            x16rs::block_hash_repeat(crate::x16rs_gate::REPEAT16_HEIGHT),
            crate::x16rs_gate::REPEAT16_HEIGHT,
            outcome.corpus.segments,
            outcome.corpus.segment_nonces,
            outcome.corpus.total_nonces(),
            outcome.corpus.headers,
            outcome.corpus.nonce_start,
            outcome.shape.work_groups,
            outcome.shape.local_size,
            outcome.shape.unit_size,
            outcome.shape.nonces(),
            outcome.pick.profile,
            outcome.objective.label(),
            request.mode.label(),
            outcome.watts_source.label(),
            crate::bench_mainnet_repeat16::fmt_rate(outcome.winner.hashrate),
            crate::bench_mainnet_repeat16::fmt_rate(input.valid_hps()),
            stale_fraction(input.mean_batch_seconds) * 100.0,
            outcome.winner.p50_ms(),
            outcome.winner.p95_ms(),
            P95_BATCH_CEILING_MS,
            match outcome.winner.telemetry.watts {
                Some(watts) => format!(
                    "{watts:.0} W {} ({} samples)",
                    outcome.watts_source.label(),
                    outcome.winner.telemetry.samples
                ),
                None => format!("{:.0} W estimated, this card reports none", request.estimated_watts),
            },
            outcome
                .peak_temp_c
                .map(|t| format!("peaked at {t:.0} C"))
                .unwrap_or_else(|| "not measured".to_string()),
            outcome
                .winner
                .telemetry
                .temp_c
                .map(|t| format!("{t:.0} C"))
                .unwrap_or_else(|| "not measured".to_string()),
            outcome.ceiling.describe(outcome.sensors),
            outcome.final_proof.thresholds,
            outcome.final_proof.window,
            outcome.final_proof.miss_probability,
            outcome.winner.cpu_checks,
        );
        // Said in the proof block and not only in the log, because the log
        // scrolls and this is the sentence that decides whether the operator's
        // chosen mode meant anything.
        if outcome.watts_source == WattsSource::Estimated
            && outcome.objective != Objective::ValidHashrate
        {
            text.push_str(
                "  mode not honoured: nothing here reports this card's power draw, so every \
                 candidate was\n                     scored on the same estimated watts and this \
                 ranking is identical to max mode's\n",
            );
        }
        if let Some(net) = input.net_eur_per_day(&request.economics) {
            text.push_str(&format!("  net              : {net:.4} EUR/day\n"));
        }
        // Why the winning shape won, where the card itself supplies the reason.
        //
        // A card sitting on its power cap and a card starved of work in flight
        // want OPPOSITE things from unit_size, and both have been measured under
        // this kernel: an RX 9070 XT gains about 9% going from 64 to 192, while a
        // Tesla T4 at 66 W against a 70 W cap loses going the same way, because
        // the larger batch cannot draw more power, only hold the card at the
        // limit for longer. The tuner finds either optimum from the hashrate
        // alone. This line is so the operator is not left to guess which regime
        // their card is in, and does not "fix" a correct answer by hand.
        if let Some(load) = outcome.power_cap_load {
            text.push_str(&format!(
                "  power limit      : {}\n",
                if load >= POWER_CAPPED_FRACTION {
                    format!(
                        "the winner drew {:.0}% of this card's {:.0} W cap, so it is POWER CAPPED. \
                         A larger\n                     launch cannot buy more work here, it only \
                         holds the card at the limit for\n                     longer; expect the \
                         smaller unit_size to win and read the batch latency above",
                        load * 100.0,
                        outcome.power_limit_w.unwrap_or_default(),
                    )
                } else {
                    format!(
                        "the winner drew {:.0}% of this card's {:.0} W cap, so it is not power \
                         limited",
                        load * 100.0,
                        outcome.power_limit_w.unwrap_or_default(),
                    )
                }
            ));
        }
        text.push_str(&format!(
            "  soak             : {} passes over {:.0}s, {}\n                     \
             p95 batch {:.0} ms on the first pass, {:.0} ms on the last\n                     \
             hashrate span {:.2}%{}{}{}\n",
            outcome.soak.len(),
            outcome.soak_seconds,
            if outcome.settle.settled {
                "settled"
            } else {
                "DID NOT SETTLE within the cap; the numbers above are the last pass"
            },
            // The tail is what a template change waits on, and it is what grows
            // when a card that started cold settles onto a power or thermal
            // limit. Reported at both ends of the soak so the growth is visible
            // rather than averaged away.
            outcome.soak.first().map(|pass| pass.p95_ms).unwrap_or(0.0),
            outcome.soak.last().map(|pass| pass.p95_ms).unwrap_or(0.0),
            outcome.settle.rate_span_pct,
            outcome
                .settle
                .temp_span_c
                .map(|v| format!(", temperature span {v:.1} C"))
                .unwrap_or_default(),
            outcome
                .settle
                .watts_span_pct
                .map(|v| format!(", power span {v:.2}%"))
                .unwrap_or_default(),
            outcome
                .settle
                .clock_span_pct
                .map(|v| format!(", clock span {v:.2}%"))
                .unwrap_or_default(),
        ));
        // What "settled" did NOT cover. Without this line a card that reports
        // nothing but a hashrate produces the same word as one that held its
        // temperature, its watts and its clock flat for five passes.
        let absent = outcome.settle.absent_signals();
        if !absent.is_empty() {
            text.push_str(&format!(
                "                     not part of that judgement, this card reports none: {}\n",
                absent.join(", ")
            ));
        }
        text.push_str(&format!(
            "  search space     : {} of {} shapes measured; {} could not meet the batch ceiling, \
             {} cost more than a {:.1}s pass{}\n",
            outcome.plan.candidates.len(),
            outcome.plan.candidates.len()
                + outcome.plan.over_ceiling.len()
                + outcome.plan.off_corpus.len(),
            outcome.plan.over_ceiling.len(),
            outcome.plan.off_corpus.len(),
            outcome.plan.pass_ceiling_seconds,
            match outcome.plan.budget_for_every_shape {
                Some(seconds) if !outcome.plan.off_corpus.is_empty() =>
                    format!(" (benchmark_seconds = {seconds} would measure them)"),
                _ => String::new(),
            }
        ));
        text.push_str(&format!(
            "  planned / actual : sweep {:.0}s planned, {:.0}s of tune in total\n",
            outcome.plan.sweep_seconds, outcome.total_seconds
        ));
        text.push_str(&resolution_note(&outcome.finalists, outcome.backend));
        text
    }
}

#[cfg(any(feature = "ocl", feature = "cuda"))]
pub use device::{
    Measured, POWER_CAPPED_FRACTION, ShapeProof, TelemetryWindow, TuneOutcome, TuneRequest,
    TuneTarget, agree_on_segments, prove_shape, render, run_corpus, tune, tune_on,
    within_temperature_limit,
};

/// The last thing a candidate has to survive before its number is believed:
/// hashing the same work as the reference and agreeing with it hash for hash.
///
/// Exposed outside `device` so the corpus tests can name it.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
pub fn coverage_matches(corpus: &Corpus, reference: Shape, other: Shape) -> Result<(), String> {
    let want = corpus.coverage_signature(reference)?;
    let got = corpus.coverage_signature(other)?;
    if want != got {
        return Err(format!(
            "shape {}x{}x{} covers the corpus as {want:?}, shape {}x{}x{} as {got:?}",
            reference.work_groups,
            reference.local_size,
            reference.unit_size,
            other.work_groups,
            other.local_size,
            other.unit_size
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(work_groups: u32, unit_size: u32) -> Shape {
        Shape {
            work_groups,
            local_size: 256,
            unit_size,
        }
    }

    /// Every (header_index, nonce) pair `shape` will hash, in the order it will
    /// hash them. The literal object the corpus is supposed to hold constant.
    fn expand(corpus: &Corpus, shape: Shape) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        for batch in corpus.batches(shape).unwrap() {
            for offset in 0..batch.nonces {
                out.push((batch.header_index, batch.nonce_start + offset as u32));
            }
        }
        out
    }

    #[test]
    fn every_candidate_hashes_exactly_the_same_header_and_nonce_pairs() {
        // This is the property the whole module exists for: two shapes with very
        // different batch sizes must cover the identical multiset of work, or
        // the faster-looking one may simply have drawn cheaper algorithms.
        let shapes = vec![shape(32, 32), shape(48, 96), shape(64, 192)];
        let (corpus, usable, dropped) =
            plan_corpus(&shapes, 0x2000_0000, 4, 4, 1 << 21, 1 << 27).unwrap();
        assert!(dropped.is_empty(), "no shape should have been dropped");
        assert_eq!(usable.len(), shapes.len());

        let reference = expand(&corpus, shapes[0]);
        assert_eq!(reference.len() as u64, corpus.total_nonces());
        for other in &shapes[1..] {
            assert_eq!(
                reference,
                expand(&corpus, *other),
                "shape {other:?} does not hash the same work as {:?}",
                shapes[0]
            );
        }
    }

    /// The closed form is allowed to stand in for the expansion only because it
    /// is the same statement.
    ///
    /// A real corpus is tens of millions of nonces and a real card offers up to
    /// forty-five shapes, so the pair-by-pair comparison above cannot be run on
    /// what the tuner actually plans: it is gigabytes per shape. Every other
    /// test therefore compares `coverage_signature`. This test is what makes
    /// that legitimate: over every grid the presets can produce, scaled down by
    /// taking `local_size = 1` so the pairs fit in memory, signatures are equal
    /// exactly when the expansions are equal, and unequal exactly when they are
    /// not.
    ///
    /// `local_size` is a common factor of every batch on a device, so scaling it
    /// changes every batch by the same factor and leaves the divisibility
    /// structure the corpus is built on identical. Nothing about the tiling
    /// depends on its value.
    #[test]
    fn identical_coverage_is_exactly_an_identical_signature() {
        let mut compared = 0usize;
        for (min_wg, max_wg, max_us) in [
            (32u32, 64u32, 192u32), // the RX 9070 XT window, as shipped
            (256, 1024, 128),       // an rx6600 / rtx4060 / arc_a380 window
            (256, 2048, 128),       // an rx6800xt / arc_a770 window
            (256, 256, 128),        // the collapsed single work-group window
        ] {
            // local_size 1, so a whole corpus is a few hundred thousand pairs.
            let universe = candidate_universe(min_wg, max_wg, max_us, 1);
            let (corpus, usable, _) =
                plan_corpus(&universe, 0, 3, 3, 1, u32::MAX as u64 / 4).unwrap();
            let reference = usable[0];
            let want = expand(&corpus, reference);
            assert_eq!(want.len() as u64, corpus.total_nonces());
            for other in &usable {
                let got = expand(&corpus, *other);
                let signatures_agree = corpus.coverage_signature(reference).unwrap()
                    == corpus.coverage_signature(*other).unwrap();
                assert_eq!(
                    got == want,
                    signatures_agree,
                    "{min_wg}..={max_wg} x {max_us}: shape {other:?} expands to {} pairs and its \
                     signature {} the reference's; the two forms disagree",
                    got.len(),
                    if signatures_agree { "equals" } else { "differs from" }
                );
                assert!(got == want, "shape {other:?} does not hash the reference's work");
                assert!(coverage_matches(&corpus, reference, *other).is_ok());
                compared += 1;
            }

            // And the negative half: a corpus placed somewhere else, or cut into
            // a different number of segments, must make both forms disagree
            // together. Without this the test would pass on a signature function
            // that returned a constant.
            let moved = Corpus {
                nonce_start: corpus.nonce_start + corpus.segment_nonces as u32,
                ..corpus
            };
            assert_ne!(expand(&moved, reference), want);
            assert_ne!(
                moved.coverage_signature(reference).unwrap(),
                corpus.coverage_signature(reference).unwrap()
            );
        }
        assert!(compared >= 40, "only {compared} shapes were compared");
    }

    /// A signature is only worth anything because the coverage it summarises is
    /// checked to be a gapless, non-overlapping, in-order cover as it is built.
    #[test]
    fn a_coverage_with_a_gap_or_an_overlap_is_refused_rather_than_summarised() {
        let corpus = Corpus {
            nonce_start: 1_000,
            headers: 2,
            segment_nonces: 4_096,
            segments: 4,
        };
        let fits = Shape {
            work_groups: 1,
            local_size: 1,
            unit_size: 1_024,
        };
        assert!(corpus.coverage_signature(fits).is_ok());
        // Four segments, two headers, so the signature is four runs and not one:
        // consecutive nonces under different headers must not be merged.
        assert_eq!(corpus.coverage_signature(fits).unwrap().len(), 4);
        assert_eq!(
            corpus
                .coverage_signature(fits)
                .unwrap()
                .iter()
                .map(|run| run.0)
                .collect::<Vec<_>>(),
            vec![0, 1, 0, 1]
        );
        // A batch that does not divide the segment cannot produce a cover at
        // all, so there is nothing to summarise.
        let does_not_tile = Shape {
            work_groups: 1,
            local_size: 1,
            unit_size: 3_000,
        };
        assert!(corpus.coverage_signature(does_not_tile).is_err());
    }

    #[test]
    fn a_shape_that_cannot_tile_the_corpus_is_refused_not_truncated() {
        let corpus = Corpus {
            nonce_start: 0,
            headers: 2,
            segment_nonces: 1_000,
            segments: 2,
        };
        let odd = shape(3, 7);
        assert!(!corpus.fits(odd));
        assert!(corpus.batches(odd).is_err());
    }

    #[test]
    fn the_corpus_quantum_is_the_least_common_multiple_and_is_capped() {
        assert_eq!(shared_segment_nonces(&[4, 6], 1, 1_000), Some(12));
        assert_eq!(shared_segment_nonces(&[4, 6], 100, 1_000), Some(108));
        assert_eq!(shared_segment_nonces(&[4, 6], 1, 11), None);
        // A batch size with a large prime factor is what blows the quantum up.
        assert_eq!(shared_segment_nonces(&[1 << 20, 7 << 20], 1, 1 << 22), None);
    }

    #[test]
    fn planning_drops_the_shape_that_forces_the_quantum_up_and_says_which() {
        // The awkward shape has the SMALLER batch (216 832 against 262 144), so
        // a planner that dropped by size would keep it and throw away the useful
        // one. What matters is that 121 = 11^2 multiplies the quantum by 847.
        let good = shape(32, 32);
        let awkward = Shape {
            work_groups: 7,
            local_size: 256,
            unit_size: 121,
        };
        assert!(awkward.nonces() < good.nonces());
        let (corpus, usable, dropped) =
            plan_corpus(&[good, awkward], 0, 1, 1, 1 << 20, 1 << 24).unwrap();
        assert_eq!(dropped, vec![awkward]);
        assert_eq!(usable, vec![good]);
        assert!(corpus.fits(good));
        assert!(!corpus.fits(awkward));
    }

    #[test]
    fn both_tuning_axes_stay_on_the_grid_that_keeps_the_corpus_small() {
        // Every point must be 2^a or 3*2^a. One value with a factor of 5 or 7
        // multiplies the corpus quantum by that factor for every candidate.
        let dyadic = |value: u32| {
            let mut v = value;
            while v % 2 == 0 {
                v /= 2;
            }
            v == 1 || v == 3
        };
        for (min, max) in [(32u32, 64u32), (256, 2048), (1, 1), (100, 100)] {
            for value in work_group_grid(min, max) {
                assert!(value >= min.min(max) && value <= max);
                if value != max {
                    assert!(dyadic(value), "{value} is not 2^a or 3*2^a");
                }
            }
        }
        assert_eq!(work_group_grid(32, 64), vec![32, 48, 64]);
        assert_eq!(unit_size_grid(192), vec![32, 48, 64, 96, 128, 192]);
        // A window containing no grid point still yields the one shape the
        // device can actually run.
        assert_eq!(work_group_grid(100, 100), vec![100]);
    }

    #[test]
    fn the_coarse_sweep_is_half_the_grid_and_refinement_fills_the_gaps() {
        let coarse = coarse_candidates(32, 64, 192, 256);
        let universe = candidate_universe(32, 64, 192, 256);
        assert!(coarse.len() < universe.len(), "the coarse pass must be coarse");
        assert!(coarse.iter().all(|shape| universe.contains(shape)));
        // Refining around any coarse point reaches only grid points, and reaches
        // at least one the coarse pass skipped.
        let base = coarse[coarse.len() / 2];
        let refined = refine_candidates(base, 32, 64, 192);
        assert!(refined.iter().all(|shape| universe.contains(shape)));
        assert!(refined.iter().any(|shape| !coarse.contains(shape)));
    }

    #[test]
    fn stale_work_is_priced_from_the_block_interval() {
        // A one-second batch at a 300-second block target throws away half a
        // second every time the job changes.
        assert!((stale_fraction(1.0) - 1.0 / 600.0).abs() < 1e-12);
        assert_eq!(stale_fraction(0.0), 0.0);
        assert_eq!(stale_fraction(f64::NAN), 0.0);
        // A slower shape with a much longer batch can lose to a faster one.
        let short = sustained_valid_hps(1_000_000.0, 0.03);
        let long = sustained_valid_hps(1_002_000.0, 60.0);
        assert!(short > long, "{short} vs {long}");
    }

    #[test]
    fn a_shape_over_the_latency_ceiling_is_refused_however_fast_it_is() {
        let econ = Economics::default();
        let fast_but_laggy = ScoreInput {
            hashrate: 100e6,
            mean_batch_seconds: 3.0,
            p95_batch_ms: 4_000.0,
            gpu_watts: 200.0,
        };
        assert_eq!(
            score(
                &fast_but_laggy,
                Objective::ValidHashrate,
                &econ,
                P95_BATCH_CEILING_MS
            ),
            None
        );
        let ordinary = ScoreInput {
            p95_batch_ms: 40.0,
            ..fast_but_laggy
        };
        assert!(
            score(
                &ordinary,
                Objective::ValidHashrate,
                &econ,
                P95_BATCH_CEILING_MS
            )
            .is_some()
        );
    }

    #[test]
    fn eco_ranks_on_measured_joules_and_max_ranks_on_hashes() {
        let econ = Economics {
            cpu_watts: 0.0,
            ..Economics::default()
        };
        let big = ScoreInput {
            hashrate: 19e6,
            mean_batch_seconds: 0.03,
            p95_batch_ms: 35.0,
            gpu_watts: 291.0,
        };
        let small = ScoreInput {
            hashrate: 6e6,
            mean_batch_seconds: 0.03,
            p95_batch_ms: 35.0,
            gpu_watts: 156.0,
        };
        let s = |input: &ScoreInput, objective| {
            score(input, objective, &econ, P95_BATCH_CEILING_MS).unwrap()
        };
        assert!(s(&big, Objective::ValidHashrate) > s(&small, Objective::ValidHashrate));
        // 19/291 = 65 kH/J against 6/156 = 38 kH/J, so Max and Eco agree here.
        assert!(s(&big, Objective::HashesPerJoule) > s(&small, Objective::HashesPerJoule));
        // A hypothetical shape that buys 5% more hashes for 60% more watts is
        // the case the two objectives must disagree on.
        let greedy = ScoreInput {
            hashrate: 19.95e6,
            gpu_watts: 465.0,
            ..big
        };
        assert!(s(&greedy, Objective::ValidHashrate) > s(&big, Objective::ValidHashrate));
        assert!(s(&greedy, Objective::HashesPerJoule) < s(&big, Objective::HashesPerJoule));
    }

    #[test]
    fn profit_needs_a_price_for_a_hash_and_says_so_when_it_has_none() {
        let priced = Economics {
            power_cost_kwh: 0.30,
            hac_price: 2.0,
            hac_per_hps_day: Some(1e-9),
            cpu_watts: 0.0,
        };
        assert_eq!(
            resolve_objective(EfficiencyMode::Profit, &priced),
            (Objective::NetIncome, None)
        );
        let no_difficulty = Economics {
            hac_per_hps_day: None,
            ..priced
        };
        let (objective, reason) = resolve_objective(EfficiencyMode::Profit, &no_difficulty);
        assert_eq!(objective, Objective::ValidHashrate);
        assert!(reason.unwrap().contains("network difficulty"));
        let no_price = Economics {
            hac_price: 0.0,
            ..no_difficulty
        };
        let (objective, reason) = resolve_objective(EfficiencyMode::Profit, &no_price);
        assert_eq!(objective, Objective::ValidHashrate);
        assert!(reason.unwrap().contains("hac_price"));
        // Eco and Max never depend on a price.
        assert_eq!(
            resolve_objective(EfficiencyMode::Eco, &no_price).0,
            Objective::HashesPerJoule
        );
        assert_eq!(
            resolve_objective(EfficiencyMode::Max, &no_price).0,
            Objective::ValidHashrate
        );
    }

    #[test]
    fn net_income_prefers_the_shape_that_earns_more_than_it_burns() {
        // Electricity at a price where the extra 174 W costs more than the extra
        // 0.95 MH/s earns, so Profit must pick the smaller shape even though Max
        // would not.
        let econ = Economics {
            power_cost_kwh: 1.0,
            hac_price: 1.0,
            hac_per_hps_day: Some(1e-9),
            cpu_watts: 0.0,
        };
        let modest = ScoreInput {
            hashrate: 19e6,
            mean_batch_seconds: 0.03,
            p95_batch_ms: 35.0,
            gpu_watts: 291.0,
        };
        let greedy = ScoreInput {
            hashrate: 19.95e6,
            gpu_watts: 465.0,
            ..modest
        };
        let net = |input: &ScoreInput| input.net_eur_per_day(&econ).unwrap();
        assert!(net(&modest) > net(&greedy), "{} vs {}", net(&modest), net(&greedy));
        assert!(greedy.valid_hps() > modest.valid_hps());
    }

    #[test]
    #[cfg(feature = "ocl")]
    fn a_shape_that_runs_the_card_past_the_operators_ceiling_is_not_a_candidate() {
        // The peak, not the mean: a shape that averages 78 C by spending part of
        // the run at 91 C has been above an 85 C ceiling.
        let hot = TelemetryWindow {
            temp_c: Some(78.0),
            peak_temp_c: Some(91.0),
            watts: Some(336.0),
            clock_mhz: Some(3_300.0),
            samples: 40,
        };
        let error = within_temperature_limit(&hot, Some(85.0)).unwrap_err();
        assert!(error.contains("91"), "{error}");
        assert!(error.contains("max_temp_c"), "{error}");
        assert_eq!(
            within_temperature_limit(&hot, Some(95.0)).unwrap(),
            TempWindow::Under {
                peak_c: 91.0,
                limit_c: 95.0
            }
        );
        // No ceiling set: nothing to enforce, and never a refusal invented out
        // of a missing measurement.
        assert_eq!(
            within_temperature_limit(&hot, None).unwrap(),
            TempWindow::NoCeiling
        );
        // A ceiling set and nothing measured is its own answer. It used to be
        // `Ok(())`, indistinguishable from a shape that stayed cool, which is
        // how "refuses a candidate past max_temp_c" did nothing on a card with
        // no thermometer.
        assert_eq!(
            within_temperature_limit(
                &TelemetryWindow {
                    peak_temp_c: None,
                    ..hot
                },
                Some(60.0)
            )
            .unwrap(),
            TempWindow::NotMeasured { limit_c: 60.0 }
        );
    }

    /// An absent thermometer is a state the tune refuses on, not a satisfied
    /// ceiling.
    ///
    /// This is defect 2 in full: `detect_gpu_temp_sensor` returns `None` for
    /// Intel and Unknown, so `Sampler` samples nothing, every window's
    /// `peak_temp_c` is `None`, and the old check compared nothing and passed.
    #[test]
    fn a_ceiling_with_no_thermometer_is_never_silently_satisfied() {
        // Intel: no source at all, so nothing on this machine could enforce it.
        let intel = TempCeiling::resolve(Some(85.0), false);
        assert_eq!(intel, TempCeiling::Unenforceable { limit_c: 85.0 });
        assert!(!intel.is_enforceable());
        let said = intel.describe("no GPU sensor on this machine");
        assert!(said.contains("CANNOT BE ENFORCED"), "{said}");
        assert!(said.contains("max_temp_c"), "{said}");
        assert!(said.contains("85"), "{said}");

        // The same card with no ceiling asked for is not a problem and must not
        // be reported as one.
        assert_eq!(TempCeiling::resolve(None, false), TempCeiling::NotRequested);
        assert!(TempCeiling::resolve(None, false).is_enforceable());
        assert!(
            !TempCeiling::resolve(None, false)
                .describe("no GPU sensor on this machine")
                .contains("CANNOT")
        );

        // A card with a thermometer enforces it, and says which sensor does.
        let amd = TempCeiling::resolve(Some(85.0), true);
        assert_eq!(amd, TempCeiling::Enforced { limit_c: 85.0 });
        assert!(amd.is_enforceable());
        assert!(amd.describe("AMD driver (ADL)").contains("AMD driver (ADL)"));

        // And every window state is distinguishable, so no caller can read
        // "nothing was measured" as "it stayed under".
        assert_eq!(
            temp_window_state(None, Some(85.0)).unwrap(),
            TempWindow::NotMeasured { limit_c: 85.0 }
        );
        assert_ne!(
            temp_window_state(None, Some(85.0)).unwrap(),
            temp_window_state(Some(70.0), Some(85.0)).unwrap()
        );
        assert!(temp_window_state(Some(85.1), Some(85.0)).is_err());
        assert_eq!(
            temp_window_state(Some(85.0), Some(85.0)).unwrap(),
            TempWindow::Under {
                peak_c: 85.0,
                limit_c: 85.0
            },
            "the ceiling is a limit, not an exclusive bound"
        );
    }

    #[test]
    fn settling_needs_every_sensor_the_card_has_to_be_flat() {
        let limits = SettleLimits::default();
        let flat: Vec<SoakPass> = (0..6)
            .map(|i| SoakPass {
                seconds: 1.0,
                hashrate: 19_000_000.0 + i as f64 * 1_000.0,
                p95_ms: 320.0,
                temp_c: Some(76.0),
                watts: Some(291.0),
                clock_mhz: Some(3_300.0),
            })
            .collect();
        assert!(settle_state(&flat, &limits).settled);

        // Still climbing in temperature: not settled, even though the hashrate
        // has stopped moving. This is the case the old 5-second verification
        // could not see.
        let mut climbing = flat.clone();
        for (i, pass) in climbing.iter_mut().enumerate() {
            pass.temp_c = Some(60.0 + i as f32 * 3.0);
        }
        assert!(!settle_state(&climbing, &limits).settled);

        // Power still ramping.
        let mut ramping = flat.clone();
        for (i, pass) in ramping.iter_mut().enumerate() {
            pass.watts = Some(200.0 + i as f32 * 20.0);
        }
        assert!(!settle_state(&ramping, &limits).settled);

        // Clock still boosting down.
        let mut boosting = flat.clone();
        for (i, pass) in boosting.iter_mut().enumerate() {
            pass.clock_mhz = Some(3_400.0 - i as f32 * 40.0);
        }
        assert!(!settle_state(&boosting, &limits).settled);

        // Too few passes is never settled.
        assert!(!settle_state(&flat[..2], &limits).settled);

        // A card with no power or clock sensor still settles on what it has,
        // and says which signals it did not have. Settling on them is not
        // optional: an NVIDIA card with no board-power sensor would otherwise
        // soak to the cap on every single run. Saying so is, and it is the
        // difference between "settled over four signals" and "settled over one"
        // wearing the same word.
        let sparse: Vec<SoakPass> = flat
            .iter()
            .map(|pass| SoakPass {
                watts: None,
                clock_mhz: None,
                ..*pass
            })
            .collect();
        let sparse_state = settle_state(&sparse, &limits);
        assert!(sparse_state.settled);
        assert_eq!(sparse_state.absent_signals(), vec!["board power", "shader clock"]);

        // An Intel card: nothing but a hashrate.
        let blind: Vec<SoakPass> = sparse
            .iter()
            .map(|pass| SoakPass {
                temp_c: None,
                ..*pass
            })
            .collect();
        let blind_state = settle_state(&blind, &limits);
        assert!(blind_state.settled);
        assert_eq!(
            blind_state.absent_signals(),
            vec!["temperature", "board power", "shader clock"]
        );

        // A card that reported all four claims nothing extra.
        assert!(settle_state(&flat, &limits).absent_signals().is_empty());
    }

    #[test]
    fn the_candidate_grid_stays_inside_the_device_limits() {
        // RX 9070 XT: work groups capped at 64, unit size at 192.
        let candidates = coarse_candidates(32, 64, 192, 256);
        assert!(!candidates.is_empty());
        assert!(
            candidates
                .iter()
                .all(|s| s.work_groups >= 32 && s.work_groups <= 64 && s.unit_size <= 192)
        );
        assert!(candidates.iter().any(|s| s.work_groups == 64));
        assert!(candidates.iter().any(|s| s.unit_size == 32));
        // A device whose cap is below a grid point must not be offered it.
        let small = coarse_candidates(32, 64, 64, 256);
        assert!(small.iter().all(|s| s.unit_size <= 64));
    }

    /// Same rule the tuner uses; duplicated here rather than exported because a
    /// test that computed the cap by calling the code under test would pass
    /// whatever that code did.
    fn cap_for(universe: &[Shape], nonce_start: u32) -> u64 {
        let largest = universe.iter().map(|s| s.nonces()).max().unwrap();
        (largest * 32).min((u32::MAX as u64 - nonce_start as u64) / 4)
    }

    #[test]
    fn every_device_shape_this_tuner_can_reach_shares_one_corpus() {
        // Not just this card. If any of these device classes starts dropping
        // candidates, the grid is wrong and some operator's tune silently
        // measures fewer shapes than it reports.
        for (min_wg, max_wg, max_us) in [
            (32u32, 64u32, 192u32),  // RX 9070 XT / RDNA4
            (256, 2048, 128),        // a large AMD or NVIDIA card
            (256, 1024, 96),         // a small discrete card
            (256, 512, 128),         // an Intel Arc
        ] {
            let universe = candidate_universe(min_wg, max_wg, max_us, 256);
            let (corpus, usable, dropped) = plan_corpus(
                &universe,
                0x2000_0000,
                4,
                4,
                1 << 21,
                cap_for(&universe, 0x2000_0000),
            )
            .unwrap();
            assert!(
                dropped.is_empty(),
                "device {min_wg}..{max_wg} x {max_us} dropped {dropped:?}"
            );
            assert_eq!(usable.len(), universe.len());
            assert!(usable.iter().all(|shape| corpus.fits(*shape)));
            // And the corpus still fits the 32-bit nonce space it is placed in.
            assert!(corpus.batches(usable[0]).is_ok());
        }
    }

    #[test]
    fn the_corpus_segment_stays_a_handful_of_batches_on_this_card() {
        // The quantum is what decides how short a candidate's measurement can
        // be. On the 9070 XT grid it must stay within a few batches of the
        // largest shape, or every measurement is padded with work nobody needs.
        let universe = candidate_universe(32, 64, 192, 256);
        let (corpus, _, _) = plan_corpus(
            &universe,
            0x2000_0000,
            4,
            4,
            1 << 21,
            cap_for(&universe, 0x2000_0000),
        )
        .unwrap();
        // Both axes are 2^a or 3*2^a, so every batch is 2^k*3^j with j <= 2 and
        // the quantum cannot exceed nine times the largest batch. Measured on
        // this grid it is six times: 64x128 contributes the 2^13 and 48x192 the
        // 3^2.
        let largest = universe.iter().map(|s| s.nonces()).max().unwrap();
        assert!(
            corpus.segment_nonces <= largest * 9,
            "segment {} against a largest batch of {largest}",
            corpus.segment_nonces
        );
        assert_eq!(corpus.segment_nonces, 18_874_368);
    }

    #[test]
    fn percentiles_are_nearest_rank_and_survive_short_samples() {
        let sorted = vec![10.0, 20.0, 30.0, 40.0, 100.0];
        assert_eq!(percentile(&sorted, 0.0), 10.0);
        assert_eq!(percentile(&sorted, 0.5), 30.0);
        assert_eq!(percentile(&sorted, 0.95), 100.0);
        assert_eq!(percentile(&[], 0.5), 0.0);
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
    }

    #[test]
    fn the_written_profile_reflects_how_much_of_the_card_the_shape_uses() {
        let vendor = crate::gpu_arch::GpuVendor::Amd;
        let full = profile_for_shape(vendor, shape(64, 192), 64, 192);
        let tiny = profile_for_shape(vendor, shape(32, 32), 64, 192);
        assert_eq!(full, "amd_max");
        assert_eq!(tiny, "amd_eco");
        let pick = pick_for_shape(vendor, shape(48, 96), 64, 192);
        assert_eq!(pick.workgroups, 48);
        assert_eq!(pick.unitsize, 96);
    }

    // -----------------------------------------------------------------------
    // Every card the code knows, driven through the candidate generator.
    //
    // The tuner was measured on one card. These tests exist so that the search
    // space every OTHER card gets is at least checked arithmetically: that it
    // is not empty, not a single point, not so coarse that the tune is a
    // formality, and not so expensive that it cannot finish.
    // -----------------------------------------------------------------------

    /// Compute-unit counts to drive each preset with.
    ///
    /// `initialize_opencl` runs the configured work_groups through
    /// `tune_workgroups`, which scales by the device's compute units, so the
    /// number the tuner sees is not the number the panel wrote. 8 is an Arc
    /// A310/A380, 170 is an RTX 5090; the rest are between.
    const COMPUTE_UNITS: [u32; 9] = [8, 16, 20, 32, 40, 60, 84, 128, 170];

    /// Bytes of device state per nonce in flight.
    ///
    /// `buffer_global_hashes` is 32 bytes per nonce and `buffer_global_order` is
    /// 4, both sized `unit_size * work_groups * local_size`; everything else the
    /// context holds is a fixed handful of kilobytes. See
    /// `opencl_gpu::resources`. This is the number that makes 64x256x192 come
    /// to the 113 MB quoted on `ArchLimits::max_unit_size`.
    const DEVICE_BYTES_PER_NONCE: u64 = 36;

    /// The window `poworker::run_block_mining_benchmark` hands the generator,
    /// for one card in one mode on a device with `compute_units` CUs.
    ///
    /// It reproduces those four lines rather than calling them, because they sit
    /// behind an OpenCL probe that needs a device.
    fn tuner_window(
        slug: &str,
        base_profile: &str,
        vram_gb: u8,
        mode: EfficiencyMode,
        compute_units: u32,
    ) -> (u32, u32, u32) {
        use crate::gpu_arch::{ArchLimits, profile_vendor, tune_workgroups};

        let limits = ArchLimits::for_panel_slug(slug);
        let shipped =
            crate::panel_tuning::resolve_panel_tuning(slug, base_profile, vram_gb, mode);
        // What the probe reports, having applied CU scaling and the arch cap.
        // The VRAM clamp inside `initialize_opencl` can only lower this further,
        // and lowering it is covered by the small-CU end of the sweep.
        let probe_wg = tune_workgroups(
            shipped.work_groups,
            compute_units,
            profile_vendor(base_profile),
            limits,
        );
        (
            limits.panel_min_wg.min(probe_wg),
            probe_wg,
            limits.max_unit_size().max(32),
        )
    }

    /// Plan the corpus over a window exactly as `tune` does, and return what the
    /// operator would actually get.
    fn plan_window(min_wg: u32, max_wg: u32, max_us: u32) -> (Corpus, Vec<Shape>, Vec<Shape>) {
        let universe = candidate_universe(min_wg, max_wg, max_us, 256);
        plan_corpus(
            &universe,
            0x2000_0000,
            4,
            4,
            1 << 21,
            cap_for(&universe, 0x2000_0000),
        )
        .unwrap_or_else(|e| panic!("window {min_wg}..={max_wg} x {max_us}: {e}"))
    }

    /// No card, in any mode, on any plausible device, gets a search space that
    /// is empty, a single point, or outside the limits it was built from.
    #[test]
    fn every_card_gets_a_search_space_worth_sweeping() {
        use crate::gpu_arch::PANEL_GPU_PRESETS;

        for (slug, profile, vram) in PANEL_GPU_PRESETS {
            for mode in [
                EfficiencyMode::Eco,
                EfficiencyMode::Profit,
                EfficiencyMode::Max,
            ] {
                for cu in COMPUTE_UNITS {
                    let (min_wg, max_wg, max_us) = tuner_window(slug, profile, vram, mode, cu);
                    let where_ = format!("{slug} {mode:?} {cu} CU ({min_wg}..={max_wg} x {max_us})");

                    assert!(min_wg >= 1 && min_wg <= max_wg, "{where_}: inverted window");

                    let coarse = coarse_candidates(min_wg, max_wg, max_us, 256);
                    let universe = candidate_universe(min_wg, max_wg, max_us, 256);
                    assert!(!coarse.is_empty(), "{where_}: nothing to sweep");
                    assert!(!universe.is_empty(), "{where_}: empty universe");

                    // Three is the arithmetic floor: the unit_size axis always
                    // offers 32/64/128 whatever the card, so a card can lose its
                    // whole work-group axis and still have something to compare.
                    // Anything below that means the generator broke.
                    assert!(
                        coarse.len() >= 3,
                        "{where_}: only {} candidates",
                        coarse.len()
                    );
                    assert!(universe.len() >= coarse.len(), "{where_}");

                    for shape in &universe {
                        assert!(
                            shape.work_groups >= min_wg && shape.work_groups <= max_wg,
                            "{where_}: {shape:?} outside the work-group window"
                        );
                        assert!(
                            shape.unit_size >= 32 && shape.unit_size <= max_us,
                            "{where_}: {shape:?} outside the unit-size window"
                        );
                        assert!(shape.nonces() > 0, "{where_}: {shape:?} hashes nothing");
                    }
                    for shape in &coarse {
                        assert!(universe.contains(shape), "{where_}: {shape:?} not planned");
                    }
                    // The top of both axes is always reachable, so no card is
                    // stopped short of its own ceiling by the grid.
                    assert!(
                        coarse.iter().any(|s| s.work_groups == *universe
                            .iter()
                            .map(|u| &u.work_groups)
                            .max()
                            .unwrap()),
                        "{where_}: the coarse sweep never reaches the top work-group count"
                    );
                    assert!(
                        coarse.iter().any(|s| s.unit_size == max_us),
                        "{where_}: the coarse sweep never reaches unit_size {max_us}"
                    );
                }
            }
        }
    }

    /// Whatever the card, the shared corpus must actually exist, must be tileable
    /// by every candidate the sweep will measure, and must fit the nonce space.
    #[test]
    fn every_card_gets_a_corpus_its_coarse_sweep_can_share() {
        use crate::gpu_arch::PANEL_GPU_PRESETS;

        for (slug, profile, vram) in PANEL_GPU_PRESETS {
            for mode in [
                EfficiencyMode::Eco,
                EfficiencyMode::Profit,
                EfficiencyMode::Max,
            ] {
                for cu in COMPUTE_UNITS {
                    let (min_wg, max_wg, max_us) = tuner_window(slug, profile, vram, mode, cu);
                    let where_ = format!("{slug} {mode:?} {cu} CU ({min_wg}..={max_wg} x {max_us})");
                    // Planned the way `tune` plans it, at the one rate anyone
                    // has ever measured for this kernel.
                    let plan = plan_session(
                        min_wg,
                        max_wg,
                        max_us,
                        256,
                        MEASURED_9070XT_HPS,
                        PANEL_BUDGET_SECONDS,
                        4,
                        NONCE_BASE,
                    )
                    .unwrap_or_else(|e| panic!("{where_}: {e}"));

                    assert!(plan.is_a_comparison(), "{where_}: nothing to compare");
                    // Everything the sweep will measure has to tile the corpus,
                    // cover it exactly, and fit the 32-bit nonce space it is
                    // placed in. `coverage_signature` checks all three, and every
                    // candidate's has to be the reference's.
                    let reference = plan.candidates[0];
                    for shape in &plan.candidates {
                        assert!(
                            plan.corpus.fits(*shape),
                            "{where_}: {shape:?} cannot tile the corpus"
                        );
                        coverage_matches(&plan.corpus, reference, *shape)
                            .unwrap_or_else(|e| panic!("{where_}: {e}"));
                    }
                    // And every refinement point, since refinement measures on
                    // the same frozen corpus.
                    for shape in &plan.usable {
                        coverage_matches(&plan.corpus, reference, *shape)
                            .unwrap_or_else(|e| panic!("{where_}: refinement point {e}"));
                    }
                    // A shape is only ever dropped for one of two stated
                    // reasons, and never silently.
                    let planned = plan.candidates.len()
                        + plan.over_ceiling.len()
                        + plan.off_corpus.len();
                    let coarse = coarse_candidates(min_wg, max_wg, max_us, 256);
                    assert!(
                        planned >= coarse.len(),
                        "{where_}: {} coarse shapes went missing without a reason",
                        coarse.len() - plan.candidates.len()
                    );
                }
            }
        }
    }

    /// The corpus quantum is the least common multiple of every candidate's
    /// batch, so it decides how much work a candidate is forced to do before it
    /// can be compared with another. If it runs away, tuning that card takes
    /// absurdly long or the planner starts throwing candidates out.
    ///
    /// Both tuning axes are 2^a or 3*2^a, which bounds the quantum at nine times
    /// the largest batch on every card. That bound is the invariant; the numbers
    /// below are what it comes to in practice.
    #[test]
    fn the_corpus_quantum_stays_within_nine_batches_on_every_card() {
        use crate::gpu_arch::PANEL_GPU_PRESETS;

        for (slug, profile, vram) in PANEL_GPU_PRESETS {
            for mode in [
                EfficiencyMode::Eco,
                EfficiencyMode::Profit,
                EfficiencyMode::Max,
            ] {
                for cu in COMPUTE_UNITS {
                    let (min_wg, max_wg, max_us) = tuner_window(slug, profile, vram, mode, cu);
                    let where_ = format!("{slug} {mode:?} {cu} CU");
                    let universe = candidate_universe(min_wg, max_wg, max_us, 256);
                    let largest = universe.iter().map(|s| s.nonces()).max().unwrap();
                    let (corpus, _, _) = plan_window(min_wg, max_wg, max_us);
                    assert!(
                        corpus.segment_nonces <= largest * 9,
                        "{where_}: quantum {} against a largest batch of {largest}",
                        corpus.segment_nonces
                    );
                }
            }
        }

        // The 9x bound is real and it is not enough on its own. These are the
        // quanta the unbounded plan produces, in nonces, for the two ends of the
        // preset table. The RX 9070 XT's is 18.9 M, about 0.65 s at its measured
        // 28.8 MH/s. The largest NVIDIA preset's is 151 M, eight times as much,
        // because the bound is 9x a batch that is itself eight times bigger.
        // That is why `plan_session` caps the quantum in seconds and not in
        // batches: the grid's own arithmetic cannot bound it in a way that
        // survives a card with a wide work-group ceiling.
        //
        // 151 M was 604 M until the NVIDIA presets were derived rather than
        // guessed. `nvidia_max` named 3584 work groups, which on the one NVIDIA
        // card ever measured is 90 waves of a kernel that holds exactly one
        // resident block per multiprocessor, and a 15.6-second batch against a
        // 1.5-second latency ceiling. It now names 768. The quantum is a
        // consequence of that, not a target: see `nvidia_launch::PRESET_LADDER`.
        let quantum_for = |slug: &str, mode: EfficiencyMode, cu: u32| -> u64 {
            let (_, profile, vram) = PANEL_GPU_PRESETS
                .iter()
                .find(|(s, _, _)| *s == slug)
                .copied()
                .unwrap();
            let (min_wg, max_wg, max_us) = tuner_window(slug, profile, vram, mode, cu);
            plan_window(min_wg, max_wg, max_us).0.segment_nonces
        };
        assert_eq!(quantum_for("rx9070xt", EfficiencyMode::Max, 32), 18_874_368);
        assert_eq!(quantum_for("rtx5090", EfficiencyMode::Max, 170), 150_994_944);
    }

    /// Refinement may only ever ask for points the corpus was planned around,
    /// and only inside the device's window.
    #[test]
    fn refinement_never_leaves_the_planned_universe() {
        use crate::gpu_arch::PANEL_GPU_PRESETS;

        for (slug, profile, vram) in PANEL_GPU_PRESETS {
            for cu in COMPUTE_UNITS {
                let (min_wg, max_wg, max_us) =
                    tuner_window(slug, profile, vram, EfficiencyMode::Max, cu);
                let universe = candidate_universe(min_wg, max_wg, max_us, 256);
                for base in &universe {
                    let refined = refine_candidates(*base, min_wg, max_wg, max_us);
                    assert!(
                        refined.contains(base),
                        "{slug} {cu} CU: refining {base:?} lost the point it started from"
                    );
                    for shape in &refined {
                        assert!(
                            universe.contains(shape),
                            "{slug} {cu} CU: refining {base:?} proposed {shape:?}, which the \
                             corpus was never planned for"
                        );
                    }
                }
            }
        }
    }

    /// `max_wg` is the work_groups the device is ALREADY configured with, so the
    /// tuner's window is `[min(panel floor, configured), configured]`. It can
    /// lower work_groups and it can never raise them, on any card in the table.
    ///
    /// This is the one structural limit the RX 9070 XT result does not
    /// generalise past. That card's +50% came from raising unit_size, an axis
    /// whose ceiling comes from `ArchLimits` rather than from the running
    /// config, and in Eco and Profit it has room there. A card whose shipped
    /// unit_size is already at its ceiling has no room on either axis: its
    /// current shape is the top corner of the search space and Auto Tune can
    /// only confirm it or shrink it.
    #[test]
    fn the_tuner_can_never_search_above_the_configured_work_groups() {
        use crate::gpu_arch::PANEL_GPU_PRESETS;

        let mut boxed_in = Vec::new();
        for (slug, profile, vram) in PANEL_GPU_PRESETS {
            for mode in [
                EfficiencyMode::Eco,
                EfficiencyMode::Profit,
                EfficiencyMode::Max,
            ] {
                let shipped =
                    crate::panel_tuning::resolve_panel_tuning(slug, profile, vram, mode);
                for cu in COMPUTE_UNITS {
                    let (min_wg, max_wg, max_us) = tuner_window(slug, profile, vram, mode, cu);
                    let universe = candidate_universe(min_wg, max_wg, max_us, 256);
                    assert!(
                        universe.iter().all(|s| s.work_groups <= max_wg),
                        "{slug} {mode:?}: something above the configured work_groups"
                    );
                    assert!(
                        !universe.iter().any(|s| s.work_groups > shipped.work_groups),
                        "{slug} {mode:?} {cu} CU: the work-group axis reached above the \
                         configured {}. Good news if deliberate: update this test.",
                        shipped.work_groups
                    );
                }

                // At the card's own CU count the unit_size axis is the only one
                // that can still go up. Record the modes where it cannot.
                let (min_wg, max_wg, max_us) = tuner_window(slug, profile, vram, mode, 170);
                let universe = candidate_universe(min_wg, max_wg, max_us, 256);
                if !universe.iter().any(|s| s.unit_size > shipped.unit_size) {
                    boxed_in.push(format!("{slug} {mode:?}"));
                }
            }
        }

        // Every one of these ships at the top corner of its own search space.
        // Auto Tune on them is a strictly downward search: it cannot reproduce
        // the RX 9070 XT result, because that result was a bigger shape.
        assert_eq!(
            boxed_in,
            vec![
                "rx6600 Eco",
                "rx6600 Profit",
                "rx7600 Eco",
                "rx7600 Profit",
                "rx6800xt Max",
                "rx7900xt Max",
                "rx7900xtx Profit",
                "rx7900xtx Max",
                "rx9070xt Max",
                "arc_a380 Profit",
                "arc_a770 Max",
            ],
            "the set of cards that cannot search upward on either axis changed"
        );

        // Ten NVIDIA entries used to be on that list and none is now, which is
        // the point of deriving the presets rather than guessing them.
        //
        // The old table put every NVIDIA tier at unit_size 96 or 128 while the
        // axis ends at 128, so a tune on those cards could only confirm the
        // shipped value or move DOWN from it - and on the one NVIDIA card
        // anyone has measured the answer is down, so that direction was at
        // least the useful one. It was still a search space with the shipped
        // shape wedged in its corner, which is the same defect the RX 9070 XT
        // had in the other direction and which cost that card 50% for a year.
        // At unit_size 64 the axis runs 32 and 48 below and 96 and 128 above,
        // so a card nobody has measured is bracketed from both sides.
        assert!(
            !boxed_in.iter().any(|entry| entry.starts_with("rtx")),
            "an NVIDIA card is back in the corner of its own search space: {boxed_in:?}"
        );
        for (slug, profile, vram) in PANEL_GPU_PRESETS {
            if crate::gpu_arch::profile_vendor(profile) != crate::gpu_arch::GpuVendor::Nvidia {
                continue;
            }
            for mode in [
                EfficiencyMode::Eco,
                EfficiencyMode::Profit,
                EfficiencyMode::Max,
            ] {
                let shipped = crate::panel_tuning::resolve_panel_tuning(slug, profile, vram, mode);
                let (min_wg, max_wg, max_us) = tuner_window(slug, profile, vram, mode, 170);
                let universe = candidate_universe(min_wg, max_wg, max_us, 256);
                assert!(
                    universe.iter().any(|s| s.unit_size < shipped.unit_size),
                    "{slug} {mode:?}: nothing below the shipped unit_size {}",
                    shipped.unit_size
                );
                assert!(
                    universe.iter().any(|s| s.unit_size > shipped.unit_size),
                    "{slug} {mode:?}: nothing above the shipped unit_size {}",
                    shipped.unit_size
                );
                assert_eq!(
                    shipped.unit_size,
                    crate::nvidia_launch::MEASURED_T4_BEST_UNIT_SIZE,
                    "{slug} {mode:?} does not ship at the one measured NVIDIA optimum"
                );
            }
        }
    }

    /// The narrowest space any shipped card gets, named so it cannot regress
    /// quietly.
    ///
    /// An Arc A310/A380 has 8 Xe cores. `tune_workgroups` scales the panel's 384
    /// down to 8 x 32 = 256, which is also `panel_min_wg`, so the work-group
    /// window collapses to a single value and the tune becomes a three point
    /// sweep of the unit_size axis alone. That is still a real comparison, but
    /// it is one axis, and it is the floor of the other.
    #[test]
    fn the_smallest_intel_card_tunes_on_one_axis() {
        let (min_wg, max_wg, max_us) =
            tuner_window("arc_a380", "intel_balanced", 6, EfficiencyMode::Eco, 8);
        assert_eq!((min_wg, max_wg, max_us), (256, 256, 128));
        assert_eq!(work_group_grid(min_wg, max_wg), vec![256]);
        let coarse = coarse_candidates(min_wg, max_wg, max_us, 256);
        assert_eq!(coarse.len(), 3);
        assert!(coarse.iter().all(|s| s.work_groups == 256));
        assert_eq!(
            coarse.iter().map(|s| s.unit_size).collect::<Vec<_>>(),
            vec![32, 64, 128]
        );
    }

    /// Whatever a future `ArchLimits` hands the generator, including windows no
    /// card has today, it must still produce something to measure.
    ///
    /// The interesting case is the degenerate one. A window of a single work
    /// group is survivable, because the unit_size axis carries the comparison;
    /// a window of a single work group AND a unit_size ceiling of 32 is not,
    /// because it yields one candidate and a tune of one candidate is not a
    /// comparison, it is a report. What keeps that unreachable is the unit_size
    /// ceiling: `max_unit_size()` is 128 or 192 and `poworker` floors it at 32,
    /// so the sweep always has at least the three unit sizes 32, 64 and 128.
    /// The second half of this test is what makes that argument load bearing.
    #[test]
    fn no_device_window_however_odd_produces_an_empty_grid() {
        for max_us in [32u32, 33, 48, 64, 96, 100, 128, 192, 256] {
            for max_wg in [1u32, 2, 7, 32, 48, 63, 64, 100, 256, 1000, 4096, 8192] {
                for min_wg in [1u32, 32, 100, 256, 512, 4096] {
                    let min_wg = min_wg.min(max_wg);
                    let coarse = coarse_candidates(min_wg, max_wg, max_us, 256);
                    let universe = candidate_universe(min_wg, max_wg, max_us, 256);
                    let where_ = format!("{min_wg}..={max_wg} x {max_us}");
                    assert!(!coarse.is_empty(), "{where_}: no coarse candidates");
                    assert!(!universe.is_empty(), "{where_}: no universe");
                    assert!(
                        coarse.iter().all(|s| s.nonces() > 0),
                        "{where_}: a candidate hashes nothing"
                    );
                    assert!(
                        coarse.iter().all(|s| universe.contains(s)),
                        "{where_}: a coarse candidate is not in the universe"
                    );
                    if max_us >= 128 {
                        assert!(
                            coarse.len() >= 3,
                            "{where_}: {} candidates is not a sweep",
                            coarse.len()
                        );
                    }
                }
            }
        }

        // The unit_size ceiling can never fall into the range where the grid
        // degenerates, for any slug the detector can produce or any panel
        // preset the user can pick.
        use crate::gpu_arch::{ArchLimits, KNOWN_ARCH_SLUGS, PANEL_GPU_PRESETS};
        for slug in KNOWN_ARCH_SLUGS {
            assert!(ArchLimits::for_slug(slug).max_unit_size() >= 128, "{slug}");
        }
        for (slug, _, _) in PANEL_GPU_PRESETS {
            assert!(ArchLimits::panel_max_unit_size(slug) >= 128, "{slug}");
        }
        assert!(ArchLimits::for_slug("some_future_card").max_unit_size() >= 128);
    }

    /// Opening the device at the top of the search space must not be an
    /// allocation the card cannot make. The tuner opens once at the ceiling of
    /// the whole universe, so that shape, not the winner, is what has to fit.
    #[test]
    fn the_largest_launch_a_card_can_be_asked_for_fits_its_vram() {
        use crate::gpu_arch::PANEL_GPU_PRESETS;

        for (slug, profile, vram) in PANEL_GPU_PRESETS {
            let vram_bytes = vram as u64 * 1024 * 1024 * 1024;
            for mode in [
                EfficiencyMode::Eco,
                EfficiencyMode::Profit,
                EfficiencyMode::Max,
            ] {
                for cu in COMPUTE_UNITS {
                    let (min_wg, max_wg, max_us) = tuner_window(slug, profile, vram, mode, cu);
                    let universe = candidate_universe(min_wg, max_wg, max_us, 256);
                    let biggest = universe.iter().map(|s| s.nonces()).max().unwrap();
                    let bytes = biggest * DEVICE_BYTES_PER_NONCE;
                    assert!(
                        bytes * 4 <= vram_bytes,
                        "{slug} {mode:?} {cu} CU: the ceiling launch wants {} MB of a {vram} GB \
                         card, leaving no room for the driver and the display",
                        bytes / (1024 * 1024)
                    );
                }
            }
        }

        // A card with no preset gets its ceiling from VRAM alone, and the
        // smallest bracket must still be safe on the smallest card in it.
        use crate::gpu_arch::ArchLimits;
        for vram in [2u8, 4, 8, 16, 24, 32, 48] {
            let max_wg = ArchLimits::panel_max_work_groups("some_future_card", vram);
            let max_us = ArchLimits::panel_max_unit_size("some_future_card");
            let bytes = max_wg as u64 * 256 * max_us as u64 * DEVICE_BYTES_PER_NONCE;
            let vram_bytes = vram as u64 * 1024 * 1024 * 1024;
            if vram >= 4 {
                assert!(
                    bytes * 2 <= vram_bytes,
                    "{vram} GB fallback wants {} MB",
                    bytes / (1024 * 1024)
                );
            }
        }
    }

    /// The window the PANEL's Auto Tune button really hands the tuner.
    ///
    /// `miner-panel::config::write_poworker_benchmark_config` rewrites `[gpu]`
    /// `work_groups` to `ArchLimits::panel_max_work_groups` and `unit_size` to
    /// `panel_max_unit_size` before it runs poworker, so the benchmark window is
    /// the card's whole safe range, not the shape the miner is currently set to.
    /// `tuner_window` above models the other entry point, a hand-written ini.
    fn panel_button_window(slug: &str, base_profile: &str, vram_gb: u8, compute_units: u32) -> (u32, u32, u32) {
        use crate::gpu_arch::{ArchLimits, profile_vendor, tune_workgroups};

        let limits = ArchLimits::for_panel_slug(slug);
        let probe_wg = tune_workgroups(
            ArchLimits::panel_max_work_groups(slug, vram_gb),
            compute_units,
            profile_vendor(base_profile),
            limits,
        );
        (
            limits.panel_min_wg.min(probe_wg),
            probe_wg,
            limits.max_unit_size().max(32),
        )
    }

    /// `soak_until_settled`'s loop, with every pass assumed perfectly flat.
    ///
    /// Best case for the card: nothing here models a clock ramp or a warming
    /// fan, only the arithmetic of how many passes fit. If this says no, no real
    /// card can do better.
    fn soak_can_settle(pass_seconds: f64, budget_seconds: u64) -> bool {
        let cap = (budget_seconds as f64 * 0.5).max(90.0).min(900.0);
        let floor = 45.0f64.min(cap);
        let window = SettleLimits::default().window;
        let mut elapsed = 0.0;
        let mut passes = 0usize;
        while elapsed < cap {
            elapsed += pass_seconds;
            passes += 1;
            if passes >= window && elapsed >= floor {
                return true;
            }
        }
        false
    }

    /// The single measured x16rs repeat-16 GPU rate in this repository:
    /// 64x256x192 on an RX 9070 XT. See `ArchLimits::max_unit_size`.
    const MEASURED_9070XT_HPS: f64 = 28.8e6;

    /// What the panel writes into `[efficiency] benchmark_seconds` before it
    /// runs poworker.
    const PANEL_BUDGET_SECONDS: u64 = 90;

    /// The lowest hashrate at which a card with this window can complete a tune:
    /// plan it, sweep it, and reach the soak's settling window.
    ///
    /// Found by bisection on the real planner rather than by a formula, because
    /// the planner's answer is not monotone in an obvious way: a slower card
    /// gets a smaller quantum cap, which drops more shapes, which shrinks the
    /// quantum, which shortens the pass. Bisection over 60 halvings resolves the
    /// boundary to well under a hash per second either side of it.
    fn lowest_finishing_hps(min_wg: u32, max_wg: u32, max_us: u32, budget: u64) -> Option<f64> {
        let finishes = |hps: f64| -> bool {
            match plan_session(min_wg, max_wg, max_us, 256, hps, budget, 4, NONCE_BASE) {
                Ok(plan) => {
                    plan.is_a_comparison()
                        && plan.soak_can_settle(budget)
                        && soak_can_settle(plan.pass_seconds, budget)
                }
                Err(_) => false,
            }
        };
        let (mut low, mut high) = (1.0e3, 1.0e9);
        if !finishes(high) {
            return None;
        }
        for _ in 0..60 {
            let middle = (low + high) / 2.0;
            if finishes(middle) {
                high = middle;
            } else {
                low = middle;
            }
        }
        Some(high)
    }

    /// Every card in the table can finish a tune at a rate it could plausibly
    /// have, which is the thing the previous corpus made impossible.
    ///
    /// The chain the old corpus failed on is arithmetic, not opinion:
    ///
    ///   * a candidate can only be measured on a whole number of its own
    ///     batches, so the corpus segment is the l.c.m. of every candidate's
    ///     batch, which the 2^a / 3*2^a grid bounds at 9x the largest batch;
    ///   * the largest batch scales with `max_work_groups`, which is 64 on
    ///     gfx1201 and 1024 to 4096 everywhere else;
    ///   * `soak_until_settled` begins a pass only while `elapsed < cap`, so
    ///     five passes need four of them to fit inside
    ///     `max(budget*0.5, 90).min(900)`: under 22.5 s each on the panel's
    ///     90 s budget;
    ///   * a pass that does not fit leaves `settle.settled` false, and
    ///     `poworker::run_block_mining_benchmark` then writes nothing.
    ///
    /// Sizing the segment from the largest candidate and then forcing four of
    /// them per pass put that requirement at 3.4 MH/s for the RX 9070 XT and 54
    /// to 215 MH/s for every other preset, against the 28.8 MH/s that is the
    /// only rate anyone has measured. `plan_session` sizes the segment from the
    /// clock instead, so the requirement is now set by the smallest launch a
    /// device offers rather than by the largest one it permits.
    #[test]
    fn every_card_can_finish_a_tune_at_a_rate_it_could_plausibly_have() {
        use crate::gpu_arch::PANEL_GPU_PRESETS;

        // The bound. 10 MH/s is a third of the one measured rate and below any
        // discrete GPU in this table; a card slower than this is not a card the
        // presets describe. Every preset must be under it with room to spare.
        const DEFENSIBLE_BOUND_HPS: f64 = 10.0e6;

        let mut worst: (f64, String) = (0.0, String::new());
        for (slug, profile, vram) in PANEL_GPU_PRESETS {
            // Both entry points: the panel's Auto Tune button, which opens the
            // window to the card's whole safe range, and a hand-written ini.
            // And the high-CU end, which is where these cards sit; a low CU
            // count only shrinks the window and helps.
            for cu in COMPUTE_UNITS {
                let windows = [
                    ("panel button", panel_button_window(slug, profile, vram, cu)),
                    (
                        "hand-written ini",
                        tuner_window(slug, profile, vram, EfficiencyMode::Max, cu),
                    ),
                ];
                for (entry, (min_wg, max_wg, max_us)) in windows {
                    let where_ = format!("{slug} {entry} {cu} CU ({min_wg}..={max_wg} x {max_us})");
                    let required =
                        lowest_finishing_hps(min_wg, max_wg, max_us, PANEL_BUDGET_SECONDS)
                            .unwrap_or_else(|| {
                                panic!("{where_}: no hashrate at all completes a tune")
                            });
                    assert!(
                        required < DEFENSIBLE_BOUND_HPS,
                        "{where_}: needs {:.2} MH/s before a tune can finish",
                        required / 1e6
                    );
                    if required > worst.0 {
                        worst = (required, where_);
                    }
                }
            }
        }
        // Named, so a change that quietly doubles it fails here rather than in
        // an operator's log. 1.86 MH/s is where it sits, on an RX 7900 XTX
        // opened to 256..=3840 work groups.
        assert!(
            worst.0 < 2.5e6,
            "the hardest preset now needs {:.2} MH/s ({})",
            worst.0 / 1e6,
            worst.1
        );

        // The same arithmetic under the corpus this replaces, computed here from
        // the same windows so the improvement is measured rather than asserted.
        // The old plan's pass was four segments of the l.c.m. of the whole
        // universe, and it had to fit `max_soak_pass_seconds`.
        for (slug, profile, vram) in PANEL_GPU_PRESETS {
            let (min_wg, max_wg, max_us) = panel_button_window(slug, profile, vram, 128);
            let quantum = plan_window(min_wg, max_wg, max_us).0.segment_nonces;
            let was = (quantum * 4) as f64 / max_soak_pass_seconds(PANEL_BUDGET_SECONDS);
            let now = lowest_finishing_hps(min_wg, max_wg, max_us, PANEL_BUDGET_SECONDS).unwrap();
            if slug == "rx9070xt" {
                // The one card this was validated on is not made worse: it
                // needed 3.4 MH/s and it still needs a fraction of that.
                assert!((was - 3.355e6).abs() < 1.0e4, "{slug}: was {was}");
                assert!(now < was, "{slug}: {now} is not below {was}");
            } else {
                assert!(
                    was > 25.0e6,
                    "{slug} only needed {:.1} MH/s before; the defect being fixed has moved",
                    was / 1e6
                );
                assert!(
                    now * 10.0 < was,
                    "{slug}: {:.1} MH/s required before, {:.2} MH/s now, which is not the order \
                     of magnitude this change is supposed to be",
                    was / 1e6,
                    now / 1e6
                );
            }
        }

        // And at the one rate anyone has measured, every preset finishes.
        let mut cannot: Vec<&str> = Vec::new();
        for (slug, profile, vram) in PANEL_GPU_PRESETS {
            let (min_wg, max_wg, max_us) = panel_button_window(slug, profile, vram, 128);
            let plan = plan_session(
                min_wg,
                max_wg,
                max_us,
                256,
                MEASURED_9070XT_HPS,
                PANEL_BUDGET_SECONDS,
                4,
                NONCE_BASE,
            );
            let finished = plan.is_ok_and(|p| {
                p.is_a_comparison() && soak_can_settle(p.pass_seconds, PANEL_BUDGET_SECONDS)
            });
            if !finished {
                cannot.push(slug);
            }
        }
        assert_eq!(
            cannot,
            Vec::<&str>::new(),
            "cards that still could not finish a tune at the one measured rate"
        );
    }

    // -----------------------------------------------------------------------
    // The NVIDIA grid: can the corpus planner finish on it?
    // -----------------------------------------------------------------------

    /// Way 1. Bisection on the real planner, judged by the planner's own
    /// opinion of whether the soak can settle.
    fn required_hps_from_the_planner(
        min_wg: u32,
        max_wg: u32,
        max_us: u32,
        budget: u64,
    ) -> Option<f64> {
        bisect_required(|hps| {
            plan_session(min_wg, max_wg, max_us, 256, hps, budget, 4, NONCE_BASE)
                .is_ok_and(|plan| plan.is_a_comparison() && plan.soak_can_settle(budget))
        })
    }

    /// Way 2. Bisection on the real planner, judged by an independent
    /// re-implementation of `soak_until_settled`'s loop.
    ///
    /// `SessionPlan::soak_can_settle` is a closed-form inequality; the free
    /// `soak_can_settle` above steps the loop pass by pass. They are two
    /// different pieces of code and this is what makes them answer the same
    /// question about the same plan.
    fn required_hps_from_the_soak_loop(
        min_wg: u32,
        max_wg: u32,
        max_us: u32,
        budget: u64,
    ) -> Option<f64> {
        bisect_required(|hps| {
            plan_session(min_wg, max_wg, max_us, 256, hps, budget, 4, NONCE_BASE)
                .is_ok_and(|plan| plan.is_a_comparison() && soak_can_settle(plan.pass_seconds, budget))
        })
    }

    /// Way 3. Algebra, with no planner in it at all.
    ///
    /// A tune is a comparison, so at least two shapes have to survive together,
    /// and `plan_session` puts three separate gates between a shape and the
    /// measurement. Write all three down for the two cheapest shapes a device
    /// offers, which are its smallest work-group count at unit_size 32 and at
    /// 64: batches B and 2B, l.c.m. 2B.
    ///
    ///   (a) **The latency prune.** A shape is dropped before anything is
    ///       measured unless its batch fits
    ///       `P95_BATCH_CEILING_MS * LATENCY_HEADROOM` at the probed rate. The
    ///       second shape is the one that has to get through, so
    ///       `hps >= 2B / (1.5 * 1.6)`. On the windows the NVIDIA presets
    ///       produce this is the gate that binds, which is worth knowing: the
    ///       corpus is not what a slow card runs out of, the p95 ceiling is.
    ///   (b) **The quantum cap.** The segment is a common multiple of the kept
    ///       batches and may not exceed a pass, so `hps >= 2B / pass_ceiling`
    ///       unless `MIN_SEGMENT_NONCES` already covers it.
    ///   (c) **The soak.** One segment is the smallest corpus and the pass has
    ///       to fit `max_soak_pass_seconds`.
    ///
    /// The requirement is the largest of the three. Everything the planner adds
    /// on top of this keeps MORE shapes and makes the corpus bigger, so this is
    /// a lower bound on ways 1 and 2, and the test asserts that relationship
    /// rather than assuming it.
    fn required_hps_closed_form(min_wg: u32, max_wg: u32, max_us: u32, budget: u64) -> f64 {
        let grid = work_group_grid(min_wg, max_wg);
        let wg0 = grid.first().copied().unwrap_or(min_wg) as u64;
        let units = unit_size_grid(max_us);
        // The two cheapest points that are powers of two, which is the family
        // `coarse_axis` chooses and `plan_corpus` keeps.
        let mut pair: Vec<u64> = units
            .iter()
            .copied()
            .filter(|u| u.is_power_of_two())
            .take(2)
            .map(u64::from)
            .collect();
        if pair.len() < 2 {
            pair = units.iter().take(2).map(|u| u64::from(*u)).collect();
        }
        let batches: Vec<u64> = pair.iter().map(|unit| wg0 * 256 * unit).collect();
        let second = batches.iter().copied().max().unwrap_or(1);
        let quantum = batches
            .iter()
            .fold(1u64, |acc, batch| lcm(acc, *batch).unwrap_or(u64::MAX));
        let segment = quantum * MIN_SEGMENT_NONCES.div_ceil(quantum).max(1);

        // (a) the latency prune has to admit the second shape.
        let by_latency = second as f64 / ((P95_BATCH_CEILING_MS / 1000.0) * LATENCY_HEADROOM);
        // (b) the quantum cap has to hold their common multiple. Below
        //     MIN_SEGMENT_NONCES the cap's own floor already does.
        let expected_passes = coarse_candidates(min_wg, max_wg, max_us, 256).len() as f64
            + REFINE_PASS_ALLOWANCE as f64
            + FINAL_PASS_ALLOWANCE as f64;
        let sweep_pass = budget as f64 * SWEEP_BUDGET_SHARE / expected_passes;
        let pass_ceiling = sweep_pass.min(max_soak_pass_seconds(budget) * SOAK_PASS_MARGIN);
        let by_quantum = if quantum <= MIN_SEGMENT_NONCES {
            0.0
        } else {
            quantum as f64 / pass_ceiling
        };
        // (c) the pass has to settle.
        let by_soak = segment as f64 / max_soak_pass_seconds(budget);

        by_latency.max(by_quantum).max(by_soak)
    }

    fn bisect_required(finishes: impl Fn(f64) -> bool) -> Option<f64> {
        let (mut low, mut high) = (1.0e3, 1.0e9);
        if !finishes(high) {
            return None;
        }
        for _ in 0..60 {
            let middle = (low + high) / 2.0;
            if finishes(middle) {
                high = middle;
            } else {
                low = middle;
            }
        }
        Some(high)
    }

    /// Multiprocessor counts of the NVIDIA cards in the panel table, which is
    /// what `tune_workgroups` scales the preset by. Kept next to the slugs it
    /// belongs to in `nvidia_launch`, and asserted equal there.
    fn nvidia_sm_count(slug: &str) -> Option<u32> {
        crate::nvidia_launch::NVIDIA_PANEL_SM_COUNTS
            .iter()
            .find(|(entry, _)| *entry == slug)
            .map(|(_, sms)| *sms)
    }

    /// Every NVIDIA card, both entry points, three ways of asking what hashrate
    /// a tune needs before it can finish. A T4 does 7.54 MH/s.
    ///
    /// This is the NVIDIA half of
    /// `every_card_can_finish_a_tune_at_a_rate_it_could_plausibly_have`, done
    /// separately because the NVIDIA grid is now derived from the hardware
    /// (`nvidia_launch`) rather than from the preset table, and because a
    /// requirement above about 2 MH/s on a card that does 7.54 is a design
    /// error rather than a tight fit. It prints the table, so
    /// `cargo test -- --nocapture nvidia_grid` is the audit.
    #[test]
    fn the_corpus_planner_finishes_on_every_nvidia_grid_three_ways() {
        use crate::gpu_arch::{PANEL_GPU_PRESETS, profile_vendor, GpuVendor};

        // A T4 sustains 7.54 MH/s. A tune that needs more than this fraction of
        // it is a tune that cannot run on the one NVIDIA card ever measured.
        const NVIDIA_BOUND_HPS: f64 = 2.0e6;

        let mut rows: Vec<String> = Vec::new();
        let mut worst: (f64, String) = (0.0, String::new());
        let mut checked = 0usize;

        println!(
            "\n{:<10} {:<18} {:>4} {:>16} {:>9} {:>9} {:>9}",
            "card", "entry", "SMs", "window", "planner", "soakloop", "algebra"
        );
        for (slug, profile, vram) in PANEL_GPU_PRESETS {
            if profile_vendor(profile) != GpuVendor::Nvidia {
                continue;
            }
            let sms = nvidia_sm_count(slug)
                .unwrap_or_else(|| panic!("{slug} has no multiprocessor count"));
            // Both entry points, driven at the card's real multiprocessor count
            // as well as at the extremes of the CU sweep, because
            // `tune_workgroups` scales the preset by it.
            for cu in [8u32, sms, 170] {
                let windows = [
                    ("panel button", panel_button_window(slug, profile, vram, cu)),
                    (
                        "hand-written ini",
                        tuner_window(slug, profile, vram, EfficiencyMode::Max, cu),
                    ),
                ];
                for (entry, (min_wg, max_wg, max_us)) in windows {
                    let where_ =
                        format!("{slug} {entry} {cu} CU ({min_wg}..={max_wg} x {max_us})");
                    let planner =
                        required_hps_from_the_planner(min_wg, max_wg, max_us, PANEL_BUDGET_SECONDS)
                            .unwrap_or_else(|| panic!("{where_}: no hashrate finishes a tune"));
                    let soak_loop = required_hps_from_the_soak_loop(
                        min_wg,
                        max_wg,
                        max_us,
                        PANEL_BUDGET_SECONDS,
                    )
                    .unwrap_or_else(|| panic!("{where_}: no hashrate finishes a tune"));
                    let algebra =
                        required_hps_closed_form(min_wg, max_wg, max_us, PANEL_BUDGET_SECONDS);

                    // The two bisections ask different code the same question.
                    // They resolve to well under a hash per second, so anything
                    // beyond rounding between them is a real disagreement about
                    // when a soak settles.
                    assert!(
                        (planner - soak_loop).abs() / planner < 1e-6,
                        "{where_}: the planner says {planner:.0} H/s and the soak loop says \
                         {soak_loop:.0} H/s; two pieces of code disagree about settling"
                    );
                    // And the algebra is a floor: everything the planner adds
                    // makes the corpus bigger, never smaller.
                    assert!(
                        algebra <= planner * 1.000_001,
                        "{where_}: the closed form wants {algebra:.0} H/s, ABOVE the planner's \
                         {planner:.0}. One of the two is modelling the wrong corpus"
                    );

                    for (way, value) in [
                        ("planner", planner),
                        ("soak loop", soak_loop),
                        ("algebra", algebra),
                    ] {
                        assert!(
                            value < NVIDIA_BOUND_HPS,
                            "{where_}: {way} needs {:.2} MH/s, and a Tesla T4 does 7.54",
                            value / 1e6
                        );
                    }
                    if planner > worst.0 {
                        worst = (planner, where_.clone());
                    }
                    rows.push(format!(
                        "{:<10} {:<18} {:>4} {:>16} {:>8.2}M {:>8.2}M {:>8.2}M",
                        slug,
                        entry,
                        cu,
                        format!("{min_wg}..{max_wg}x{max_us}"),
                        planner / 1e6,
                        soak_loop / 1e6,
                        algebra / 1e6
                    ));
                    checked += 1;
                }
            }
        }
        for row in &rows {
            println!("{row}");
        }
        println!(
            "worst NVIDIA requirement {:.2} MH/s ({}), against 7.54 MH/s measured on a T4\n",
            worst.0 / 1e6,
            worst.1
        );

        assert!(checked >= 50, "only {checked} NVIDIA windows were checked");
        // Named so a change that quietly doubles it fails here rather than in an
        // operator's log.
        //
        // 1.75 MH/s is where every window the PRESETS produce sits, and it is
        // not the corpus: it is gate (a), the latency prune needing to admit
        // 256 x 256 x 64, whose 4 194 304-nonce batch has to come in under
        // 1.5 s x 1.6. 1.86 is the panel Auto Tune button on a 4090 or 5090,
        // whose window is deliberately the card's whole safe range (3584 work
        // groups) rather than the preset. A T4 does 7.54 MH/s, so the tightest
        // of these leaves a factor of four.
        assert!(
            worst.0 < 2.0e6,
            "the hardest NVIDIA window now needs {:.2} MH/s ({})",
            worst.0 / 1e6,
            worst.1
        );
        // The windows the presets themselves produce are the tighter number,
        // and they are the ones a change to `nvidia_launch::PRESET_LADDER`
        // would move.
        let preset_worst = PANEL_GPU_PRESETS
            .iter()
            .filter(|(_, profile, _)| profile_vendor(profile) == GpuVendor::Nvidia)
            .flat_map(|(slug, profile, vram)| {
                [8u32, 170].into_iter().map(move |cu| {
                    let (a, b, c) = tuner_window(slug, profile, *vram, EfficiencyMode::Max, cu);
                    required_hps_from_the_planner(a, b, c, PANEL_BUDGET_SECONDS).unwrap()
                })
            })
            .fold(0.0f64, f64::max);
        assert!(
            preset_worst < 1.8e6,
            "the NVIDIA preset windows need {:.2} MH/s",
            preset_worst / 1e6
        );

        // And end to end at the rate the card really does: a full plan, at the
        // T4's own multiprocessor count and its own measured hashrate.
        let (min_wg, max_wg, max_us) = (
            crate::nvidia_launch::work_group_floor(crate::nvidia_launch::MEASURED_T4_SM_COUNT, 1),
            7_387,
            128,
        );
        let plan = plan_session(
            min_wg,
            max_wg,
            max_us,
            256,
            crate::nvidia_launch::MEASURED_T4_HPS,
            PANEL_BUDGET_SECONDS,
            4,
            NONCE_BASE,
        )
        .expect("a T4 at its measured rate must be plannable");
        assert!(plan.is_a_comparison() && plan.soak_can_settle(PANEL_BUDGET_SECONDS));
        assert!(soak_can_settle(plan.pass_seconds, PANEL_BUDGET_SECONDS));
        // The measured optimum, 256 x 256 x 64, has to survive planning: a tune
        // that drops the answer for cost is not a tune.
        assert!(
            plan.usable.contains(&shape(
                256,
                crate::nvidia_launch::MEASURED_T4_BEST_UNIT_SIZE
            )),
            "the T4's measured winner was planned out of its own search"
        );
        println!(
            "T4 end to end: {} coarse candidates, {} usable, {:.2}s a pass at 7.54 MH/s",
            plan.candidates.len(),
            plan.usable.len(),
            plan.pass_seconds
        );
    }

    /// The multiprocessor counts live in one place.
    #[test]
    fn every_nvidia_panel_card_has_a_multiprocessor_count() {
        use crate::gpu_arch::{PANEL_GPU_PRESETS, profile_vendor, GpuVendor};
        let mut named = 0;
        for (slug, profile, _) in PANEL_GPU_PRESETS {
            if profile_vendor(profile) != GpuVendor::Nvidia {
                assert!(
                    nvidia_sm_count(slug).is_none(),
                    "{slug} is not an NVIDIA card but has an SM count"
                );
                continue;
            }
            let sms = nvidia_sm_count(slug)
                .unwrap_or_else(|| panic!("{slug} is an NVIDIA card with no multiprocessor count"));
            assert!((16..=256).contains(&sms), "{slug}: {sms} multiprocessors");
            named += 1;
        }
        assert_eq!(named, crate::nvidia_launch::NVIDIA_PANEL_SM_COUNTS.len());
    }

    /// The soak's arithmetic, from three directions that have to agree.
    ///
    /// `max_soak_pass_seconds` is what `plan_session` sizes the corpus against,
    /// `soak_can_settle` is `soak_until_settled`'s loop written out, and the
    /// closed form is the algebra. Nothing in the tuner is allowed to hold a
    /// fourth opinion about how long a pass may take.
    #[test]
    fn the_longest_settleable_pass_is_one_number_and_three_things_agree_on_it() {
        for budget in [30u64, 60, 90, 120, 300, 600, 1_800, 7_200] {
            let limit = max_soak_pass_seconds(budget);
            assert_eq!(
                limit,
                soak_cap_seconds(budget) / (soak_window_passes() as f64 - 1.0)
            );
            assert!(
                soak_can_settle(limit * 0.99, budget),
                "budget {budget}: a pass just under {limit:.2}s must settle"
            );
            assert!(
                !soak_can_settle(limit * 1.01, budget),
                "budget {budget}: a pass just over {limit:.2}s must not"
            );
            // The floor never outlasts the cap, or a soak could not end.
            assert!(soak_floor_seconds(budget) <= soak_cap_seconds(budget));
        }
        // The panel's own budget, spelled out: 90 s cap, 22.5 s a pass.
        assert_eq!(soak_cap_seconds(PANEL_BUDGET_SECONDS), 90.0);
        assert_eq!(max_soak_pass_seconds(PANEL_BUDGET_SECONDS), 22.5);
        // A larger budget really does buy a longer soak, up to the 900 s cap.
        assert_eq!(soak_cap_seconds(1_000), 500.0);
        assert_eq!(soak_cap_seconds(100_000), 900.0);
    }

    /// The plan is checked against the clock before a sweep starts, not after.
    ///
    /// The failure the old tuner had no way to see was that a pass could not fit
    /// the soak's window; it discovered that only after sweeping, and then told
    /// the operator to raise `benchmark_seconds`, which does not shorten a pass.
    /// `plan_session` cannot return a plan with that property at all.
    #[test]
    fn a_plan_that_could_not_settle_is_refused_before_anything_is_measured() {
        // Every window in the table, every plausible rate, every budget an
        // operator can set: a returned plan always fits the soak.
        use crate::gpu_arch::PANEL_GPU_PRESETS;
        for (slug, profile, vram) in PANEL_GPU_PRESETS {
            for cu in [8u32, 32, 128] {
                let (min_wg, max_wg, max_us) = panel_button_window(slug, profile, vram, cu);
                for hps in [0.5e6, 2.0e6, 10.0e6, 28.8e6, 120.0e6, 500.0e6] {
                    for budget in [30u64, 90, 300, 1_800] {
                        let where_ = format!("{slug} {cu} CU at {:.1} MH/s, {budget}s", hps / 1e6);
                        match plan_session(
                            min_wg, max_wg, max_us, 256, hps, budget, 4, NONCE_BASE,
                        ) {
                            Ok(plan) => {
                                assert!(
                                    soak_can_settle(plan.pass_seconds, budget),
                                    "{where_}: planned a {:.1}s pass, which cannot settle",
                                    plan.pass_seconds
                                );
                                assert!(plan.is_a_comparison(), "{where_}: one shape is not a tune");
                                assert!(
                                    plan.corpus.segments >= 1 && plan.corpus.headers >= 1,
                                    "{where_}: empty corpus"
                                );
                                assert!(
                                    plan.corpus.headers <= plan.corpus.segments,
                                    "{where_}: {} headers over {} segments, so some header is \
                                     never hashed",
                                    plan.corpus.headers,
                                    plan.corpus.segments
                                );
                                assert!(
                                    plan.corpus.batches(plan.candidates[0]).is_ok(),
                                    "{where_}: the corpus does not fit the nonce space"
                                );
                            }
                            // A refusal is allowed, but it has to name what the
                            // operator can do about it.
                            Err(error) => assert!(
                                error.contains("work_groups") || error.contains("benchmark_seconds"),
                                "{where_}: refused with no remedy: {error}"
                            ),
                        }
                    }
                }
            }
        }
    }

    /// What the tuner offers instead of dropping shapes silently: the budget
    /// that would have kept them, and it has to be true.
    #[test]
    fn the_budget_the_tuner_names_really_does_buy_back_the_dropped_shapes() {
        use crate::gpu_arch::PANEL_GPU_PRESETS;

        let mut checked = 0usize;
        for (slug, profile, vram) in PANEL_GPU_PRESETS {
            let (min_wg, max_wg, max_us) = panel_button_window(slug, profile, vram, 128);
            for hps in [10.0e6, 28.8e6, 60.0e6] {
                let plan =
                    plan_session(min_wg, max_wg, max_us, 256, hps, PANEL_BUDGET_SECONDS, 4, NONCE_BASE)
                        .unwrap();
                let Some(bigger) = plan.budget_for_every_shape else {
                    continue;
                };
                if plan.off_corpus.is_empty() {
                    continue;
                }
                let richer =
                    plan_session(min_wg, max_wg, max_us, 256, hps, bigger, 4, NONCE_BASE).unwrap();
                assert!(
                    richer.off_corpus.is_empty(),
                    "{slug} at {:.1} MH/s: the tuner offered benchmark_seconds = {bigger}, and at \
                     that budget it still drops {} shape(s)",
                    hps / 1e6,
                    richer.off_corpus.len()
                );
                assert!(
                    richer.usable.len() > plan.usable.len(),
                    "{slug}: the larger budget bought nothing"
                );
                assert!(
                    soak_can_settle(richer.pass_seconds, bigger),
                    "{slug}: the budget the tuner named cannot settle"
                );
                checked += 1;
            }
        }
        assert!(checked >= 10, "only {checked} cases exercised the offer");
    }

    /// The card the tuner was validated on keeps the tune it was validated with.
    ///
    /// This change exists to make every OTHER card work; if it moved the RX 9070
    /// XT's search space, the one result anyone has measured would no longer be
    /// reachable and the fix would have cost more than it bought.
    #[test]
    fn the_validated_card_still_sweeps_its_whole_grid() {
        let (min_wg, max_wg, max_us) = panel_button_window("rx9070xt", "amd_balanced", 16, 32);
        assert_eq!((min_wg, max_wg, max_us), (32, 64, 192));

        let universe = candidate_universe(min_wg, max_wg, max_us, 256);
        let coarse = coarse_candidates(min_wg, max_wg, max_us, 256);
        for hps in [10.0e6, 20.0e6, MEASURED_9070XT_HPS, 50.0e6] {
            let plan = plan_session(
                min_wg,
                max_wg,
                max_us,
                256,
                hps,
                PANEL_BUDGET_SECONDS,
                4,
                NONCE_BASE,
            )
            .unwrap();
            assert_eq!(
                plan.usable.len(),
                universe.len(),
                "at {:.1} MH/s the 9070 XT lost grid points: {:?} {:?}",
                hps / 1e6,
                plan.over_ceiling,
                plan.off_corpus
            );
            assert_eq!(plan.candidates.len(), coarse.len());
            assert!(plan.over_ceiling.is_empty() && plan.off_corpus.is_empty());
            // 64x256x192, the measured winner, is still in the search.
            assert!(plan.usable.contains(&Shape {
                work_groups: 64,
                local_size: 256,
                unit_size: 192,
            }));
        }

        // And the corpus it gets at its measured rate: the same 18 874 368-nonce
        // segment as before, four of them, a 2.6 s pass.
        let plan = plan_session(
            min_wg,
            max_wg,
            max_us,
            256,
            MEASURED_9070XT_HPS,
            PANEL_BUDGET_SECONDS,
            4,
            NONCE_BASE,
        )
        .unwrap();
        assert_eq!(plan.corpus.segment_nonces, 18_874_368);
        assert_eq!(plan.corpus.segments, 4);
        assert_eq!(plan.corpus.headers, 4);
        assert!(
            (plan.pass_seconds - 2.62).abs() < 0.05,
            "pass {:.2}s",
            plan.pass_seconds
        );
    }

    /// A shape whose batch cannot meet the p95 ceiling is dropped before it is
    /// measured, because `score` was always going to refuse it.
    #[test]
    fn a_shape_the_scorer_would_refuse_is_never_measured_in_the_first_place() {
        // A window whose largest batch is 100 663 296 nonces, on a card doing
        // 20 MH/s: that batch is 5.0 s, well over the 1.5 s ceiling.
        let plan = plan_session(256, 3072, 128, 256, 20.0e6, 90, 4, NONCE_BASE).unwrap();
        let ceiling_nonces = (P95_BATCH_CEILING_MS / 1000.0) * LATENCY_HEADROOM * 20.0e6;
        assert!(!plan.over_ceiling.is_empty(), "nothing was pruned");
        for shape in &plan.over_ceiling {
            assert!(shape.nonces() as f64 > ceiling_nonces);
            // And it really would have been refused: even at the generous
            // headroom rate, its p95 cannot come in under the ceiling.
            let best_case_ms = shape.nonces() as f64 / (20.0e6 * LATENCY_HEADROOM) * 1000.0;
            assert!(
                best_case_ms > P95_BATCH_CEILING_MS,
                "{shape:?} would have taken {best_case_ms:.0} ms, inside the ceiling"
            );
        }
        for shape in plan.usable.iter().chain(plan.candidates.iter()) {
            assert!(shape.nonces() as f64 <= ceiling_nonces, "{shape:?} survived the prune");
        }
        // The prune can never empty the set: the smallest shape is kept whatever
        // the rate, so a very slow card gets a refusal with a remedy and not a
        // panic on an empty grid.
        let crawling = plan_session(256, 3072, 128, 256, 1.0e3, 90, 4, NONCE_BASE);
        match crawling {
            Ok(plan) => assert!(!plan.candidates.is_empty()),
            Err(error) => assert!(
                error.contains("work_groups") || error.contains("benchmark_seconds"),
                "{error}"
            ),
        }
    }

    /// On a card with no board-power sensor, all three efficiency modes rank
    /// candidates in exactly the same order.
    ///
    /// `Sampler::start` only reports `measures_power` when the reading comes
    /// from `GpuTempSensorSource::AmdDriver`; `read_board_power_w` in
    /// `efficiency.rs` returns `None` for every `Command` source (nvidia-smi)
    /// and `detect_gpu_temp_sensor` returns `None` outright for Intel and
    /// Unknown. So on every NVIDIA and Intel card `ScoreInput::gpu_watts` is the
    /// same configured constant for every candidate, and both
    /// `hashes_per_joule` and `net_eur_per_day` become affine in `valid_hps`.
    ///
    /// The tuner says so in its log ("optimises ... on estimated watts") but the
    /// panel does not, and an operator who chose Eco gets Max.
    #[test]
    fn without_a_power_sensor_eco_and_profit_are_max_by_another_name() {
        let econ = Economics {
            power_cost_kwh: 0.30,
            hac_price: 12.0,
            hac_per_hps_day: Some(4.0e-9),
            cpu_watts: 40.0,
        };
        // Every mode resolves to a real objective, so nothing below is a
        // fallback to throughput for a missing price.
        assert_eq!(resolve_objective(EfficiencyMode::Eco, &econ).0, Objective::HashesPerJoule);
        assert_eq!(resolve_objective(EfficiencyMode::Profit, &econ).0, Objective::NetIncome);
        assert_eq!(resolve_objective(EfficiencyMode::Max, &econ).0, Objective::ValidHashrate);

        // The estimated-watts path: one number from [gpu] gpu_profile, reused
        // for every candidate because no sensor contradicts it.
        const ESTIMATED_WATTS: f64 = 260.0;
        let candidates: Vec<ScoreInput> = [12.0e6, 19.0e6, 24.5e6, 28.8e6, 21.0e6]
            .into_iter()
            .map(|hashrate| ScoreInput {
                hashrate,
                mean_batch_seconds: 0.12,
                p95_batch_ms: 140.0,
                gpu_watts: ESTIMATED_WATTS,
            })
            .collect();

        let order = |objective: Objective| -> Vec<usize> {
            let mut index: Vec<usize> = (0..candidates.len()).collect();
            index.sort_by(|a, b| {
                score(&candidates[*b], objective, &econ, P95_BATCH_CEILING_MS)
                    .unwrap()
                    .total_cmp(&score(&candidates[*a], objective, &econ, P95_BATCH_CEILING_MS).unwrap())
            });
            index
        };

        let by_rate = order(Objective::ValidHashrate);
        assert_eq!(order(Objective::HashesPerJoule), by_rate, "Eco is not Eco");
        assert_eq!(order(Objective::NetIncome), by_rate, "Profit is not Profit");

        // And it is only the shared constant that does it: give two candidates
        // real, different draws and the orders separate again, which is what an
        // AMD card on Windows gets and nothing else does.
        let thirsty = ScoreInput {
            gpu_watts: ESTIMATED_WATTS * 1.9,
            ..candidates[3]
        };
        let frugal = candidates[2];
        assert!(
            score(&thirsty, Objective::ValidHashrate, &econ, P95_BATCH_CEILING_MS)
                > score(&frugal, Objective::ValidHashrate, &econ, P95_BATCH_CEILING_MS)
        );
        assert!(
            score(&thirsty, Objective::HashesPerJoule, &econ, P95_BATCH_CEILING_MS)
                < score(&frugal, Objective::HashesPerJoule, &econ, P95_BATCH_CEILING_MS),
            "with real watts Eco must be able to prefer the slower shape"
        );
    }

    // -----------------------------------------------------------------------
    // The T4 measurement, and what the tuner has to be able to say about it
    // -----------------------------------------------------------------------

    /// Every launch shape measured on a real Tesla T4 is on the tuner's grid,
    /// and the tuner can reach the one that won.
    ///
    /// Measured on a Colab T4 at repeat 16 over a fixed corpus, steady state
    /// after 40 warm-up batches, flat to 0.57%:
    ///
    ///   work_groups 256, local_size 256, unit_size  64  ->  7.54 MH/s
    ///   work_groups 256, local_size 256, unit_size  96  ->  7.19 MH/s
    ///   work_groups 256, local_size 256, unit_size 128  ->  7.06 MH/s
    ///
    /// That ordering is the REVERSE of the RX 9070 XT's, where unit_size 192
    /// beats 64 by about 9%. Two cards, opposite optima, which is exactly why
    /// the grid may not be a fixed table. What this pins is that a T4-shaped
    /// device window puts all three of those points in the universe, and that
    /// the winner is reachable from a point the coarse sweep actually visits.
    #[test]
    fn the_shapes_measured_on_a_real_t4_are_on_the_grid_and_the_winner_is_reachable() {
        // A T4 as the CUDA probe describes it: 40 multiprocessors, one resident
        // block of 256 threads each, so 40 is the smallest launch that fills it.
        let min_wg = 40;
        // Roughly what 55% of ~15 GiB free holds at unit_size 128, 36 bytes a
        // nonce. The exact figure is the CUDA crate's to compute; what matters
        // here is that the window is wide.
        let max_wg = 7_000;
        let max_unit = 128;
        let universe = candidate_universe(min_wg, max_wg, max_unit, 256);

        for unit_size in [64u32, 96, 128] {
            assert!(
                universe.contains(&shape(256, unit_size)),
                "256x256x{unit_size} was measured on a T4 and must be a shape the tuner can try"
            );
        }

        // The coarse sweep is half the grid, so 256 work groups is a refinement
        // point rather than a coarse one. It must still be reachable: a tune
        // that could never propose the shape that won on the one NVIDIA card
        // anyone has measured would be a tuner in name only.
        let coarse = coarse_candidates(min_wg, max_wg, max_unit, 256);
        let reachable = coarse.iter().any(|base| {
            refine_candidates(*base, min_wg, max_wg, max_unit).contains(&shape(256, base.unit_size))
        });
        assert!(
            reachable,
            "no coarse candidate refines to 256 work groups; the T4 optimum is unreachable"
        );

        // The small end of the unit_size axis is where a power-capped card's
        // optimum lives, so the grid must not start above it.
        assert_eq!(
            unit_size_grid(max_unit).first().copied(),
            Some(32),
            "a power-capped card wants a SMALL unit_size; the axis has to start below 64"
        );
    }

    #[test]
    fn a_win_smaller_than_the_measurement_noise_is_reported_as_unresolved() {
        // Two shapes whose medians differ by well under a percent, each of which
        // wandered by about 3% across its own repeats. Nothing was demonstrated
        // here, and the tuner has to say so rather than print a winner and stop.
        let noisy = vec![
            FinalistRuns {
                shape: shape(256, 64),
                scores: vec![7.40e6, 7.54e6, 7.62e6],
            },
            FinalistRuns {
                shape: shape(256, 96),
                scores: vec![7.35e6, 7.51e6, 7.58e6],
            },
        ];
        let resolution = comparison_resolution_pct(&noisy);
        let margin = winning_margin_pct(&noisy).expect("two finalists have a margin");
        assert!(resolution > 2.0, "each shape spanned about 3% of itself");
        assert!(margin < 1.0);
        assert!(margin < resolution);
        let note = resolution_note(&noisy, "cuda");
        assert!(note.contains("does NOT clear it"), "{note}");

        // The same two medians, measured on a card that reproduced to a tenth of
        // a percent: now the margin is a result.
        let steady = vec![
            FinalistRuns {
                shape: shape(256, 64),
                scores: vec![7.539e6, 7.540e6, 7.541e6],
            },
            FinalistRuns {
                shape: shape(256, 96),
                scores: vec![7.189e6, 7.190e6, 7.191e6],
            },
        ];
        let note = resolution_note(&steady, "cuda");
        assert!(comparison_resolution_pct(&steady) < 0.1);
        assert!(winning_margin_pct(&steady).unwrap() > 4.0);
        assert!(note.contains("which clears it"), "{note}");
    }

    #[test]
    fn a_single_finalist_never_claims_a_comparison() {
        // One shape survived to the final round. There is a number, but there is
        // no comparison, and "resolved" would be a claim about nothing.
        let alone = vec![FinalistRuns {
            shape: shape(256, 64),
            scores: vec![7.54e6, 7.55e6, 7.53e6],
        }];
        assert_eq!(winning_margin_pct(&alone), None);
        let note = resolution_note(&alone, "cuda");
        assert!(note.contains("nothing was compared"), "{note}");

        // And a finalist with a single pass contributes no noise estimate: one
        // measurement cannot disagree with itself, and counting it as 0% would
        // make every margin look resolved.
        let one_pass = vec![
            FinalistRuns {
                shape: shape(256, 64),
                scores: vec![7.54e6],
            },
            FinalistRuns {
                shape: shape(256, 96),
                scores: vec![7.19e6, 7.60e6, 7.30e6],
            },
        ];
        assert!(
            comparison_resolution_pct(&one_pass) > 5.0,
            "the resolution must come from the shape that actually repeated"
        );
    }

    #[test]
    fn the_report_says_what_a_number_from_another_run_is_worth_and_says_more_on_cuda() {
        let finalists = vec![
            FinalistRuns {
                shape: shape(256, 64),
                scores: vec![7.539e6, 7.540e6, 7.541e6],
            },
            FinalistRuns {
                shape: shape(256, 96),
                scores: vec![7.189e6, 7.190e6, 7.191e6],
            },
        ];
        // Both backends carry the between-process bound, because it is a
        // property of the card settling into a clock state for the life of a
        // process and not of the GPU API.
        for backend in ["opencl", "cuda"] {
            let note = resolution_note(&finalists, backend);
            assert!(
                note.contains(&format!(
                    "{:.1}%",
                    crate::x16rs_gate::BETWEEN_PROCESS_SPREAD_PCT
                )),
                "{backend}: {note}"
            );
        }
        // Only CUDA carries the extra claim, and it is the true one: nvcc
        // compiles the kernel into the binary, so two CUDA KERNELS are two
        // binaries and `x16rs_gate ab`'s in-process alternation is not available
        // for them. Launch shapes, which is what a tune compares, are alternated
        // in-process on both backends and resolve far finer.
        let cuda = resolution_note(&finalists, "cuda");
        let opencl = resolution_note(&finalists, "opencl");
        assert!(
            cuda.contains("nvcc compiles the kernel into the binary"),
            "{cuda}"
        );
        assert!(!opencl.contains("nvcc"), "{opencl}");
        assert!(cuda.len() > opencl.len());
    }

    /// One device allocation may serve several candidates, and it is sized from
    /// the ones it will really launch.
    ///
    /// This is the rule a backend that bakes unit_size into its buffers depends
    /// on. Getting it wrong in the tempting direction - allocate for the largest
    /// shape in the plan - asks the card for memory no launch would ever use.
    #[test]
    fn a_shared_allocation_is_sized_from_the_shapes_that_share_its_unit_size() {
        let plan = vec![
            shape(3072, 32),
            shape(1536, 32),
            shape(512, 64),
            shape(256, 128),
            shape(128, 128),
        ];

        // Work groups come from the plan, so one device at unit_size 32 serves
        // both 1536 and 3072, and one at 128 serves both 128 and 256.
        assert_eq!(shared_allocation_work_groups(&plan, shape(1536, 32)), 3072);
        assert_eq!(shared_allocation_work_groups(&plan, shape(128, 128)), 256);
        assert_eq!(shared_allocation_work_groups(&plan, shape(512, 64)), 512);

        // The unit_size filter is the whole point. The largest work-group count
        // in this plan is 3072 and the largest unit_size is 128, but no planned
        // shape is 3072x256x128: allocating one would be 100 M nonces and 3.6 GB
        // of device memory for a launch that cannot happen.
        let naive_nonces = 3072u64 * 256 * 128;
        let real = shared_allocation_work_groups(&plan, shape(256, 128)) as u64 * 256 * 128;
        assert_eq!(real, 256 * 256 * 128);
        assert!(
            real * 10 < naive_nonces,
            "sizing by the plan's largest shape would over-allocate by more than 10x"
        );

        // A shape the plan never mentioned still gets an allocation it can use,
        // which is what the probe needs before a plan exists.
        assert_eq!(shared_allocation_work_groups(&[], shape(48, 32)), 48);
        assert_eq!(shared_allocation_work_groups(&plan, shape(4096, 32)), 4096);

        // local_size is part of the identity too: a device allocated for one
        // block size cannot serve another.
        let other_block = Shape {
            work_groups: 4096,
            local_size: 64,
            unit_size: 32,
        };
        assert_eq!(shared_allocation_work_groups(&[other_block], shape(256, 32)), 256);
    }

    /// A T4-shaped device window plans a real comparison at the shipped budget.
    ///
    /// This is a regression test for a failure that had nothing to do with the
    /// card. A Tesla T4 has 40 multiprocessors and this kernel gets one resident
    /// block on each, so the work-group axis starts at 48 - the first dyadic
    /// grid point at or above 40. Taking every other point from index 0 then
    /// makes EVERY coarse candidate a 3-multiple; the shared corpus segment is a
    /// common multiple of the candidates' batches, so one 3-multiple triples it;
    /// `plan_corpus` drops the shapes that will not fit the budget and drops the
    /// 3-multiples first; and the coarse set comes back empty. The operator's
    /// tune then ends with "only 0 launch shapes survived planning" on a card
    /// with nothing wrong with it.
    ///
    /// The floor is not negotiable - below one block per multiprocessor the card
    /// is idle by construction - so `coarse_axis` chooses the family instead.
    #[test]
    fn a_t4_shaped_device_window_plans_a_comparison_at_the_shipped_budget() {
        // 40 multiprocessors, one resident block each; ~7400 work groups is what
        // 55% of a T4's free memory holds at unit_size 128, 36 bytes a nonce.
        let (min_wg, max_wg, max_unit) = (40, 7_387, 128);

        // The probe runs the SMALLEST shape, which under-feeds this kernel, so it
        // reads below the 7.5 MH/s the card sustains at a good shape. Planned
        // across the range a probe could plausibly return.
        for probe_hps in [3.0e6f64, 5.0e6, 7.5e6] {
            let plan = plan_session(min_wg, max_wg, max_unit, 256, probe_hps, 90, 4, NONCE_BASE)
                .unwrap_or_else(|error| {
                    panic!("a T4 at {:.1} MH/s must be plannable: {error}", probe_hps / 1e6)
                });
            assert!(
                plan.is_a_comparison(),
                "at {:.1} MH/s the coarse set was {} shapes",
                probe_hps / 1e6,
                plan.candidates.len()
            );
            // Every planned candidate has to tile the corpus it was planned for,
            // or the shapes are not being measured on the same work.
            for shape in &plan.candidates {
                assert!(
                    plan.corpus.fits(*shape),
                    "{shape} cannot tile the {}-nonce segment it was planned onto",
                    plan.corpus.segment_nonces
                );
                coverage_matches(&plan.corpus, plan.candidates[0], *shape).unwrap();
            }
            assert!(plan.soak_can_settle(90));
            // The T4's measured optimum sits at 256 work groups, and it must be
            // a shape the sweep really measures rather than one the corpus
            // dropped for cost.
            assert!(
                plan.candidates.iter().any(|shape| shape.work_groups == 256),
                "at {:.1} MH/s no candidate had the 256 work groups the T4 was measured at",
                probe_hps / 1e6
            );
        }

        // The family chosen for a grid that starts at 48 is the one the shared
        // corpus can afford. The one exception is the axis's top end, which
        // `coarse_axis` always appends whichever family it belongs to, because
        // the largest launch the device permits is worth visiting on its own.
        let coarse = coarse_candidates(min_wg, max_wg, max_unit, 256);
        let top = work_group_grid(min_wg, max_wg).last().copied().unwrap();
        assert!(
            coarse
                .iter()
                .all(|shape| shape.work_groups.is_power_of_two() || shape.work_groups == top),
            "the coarse work-group axis must be the family the shared corpus can afford, \
             plus the grid's top end"
        );

        // A grid that starts on a power of two is untouched: this is the RX 9070
        // XT's window, and its coarse axis is what it always was.
        let amd = coarse_candidates(32, 64, 192, 256);
        let amd_axis: Vec<u32> = {
            let mut wg: Vec<u32> = amd.iter().map(|shape| shape.work_groups).collect();
            wg.dedup();
            wg
        };
        assert_eq!(amd_axis, vec![32, 64]);
    }
}
