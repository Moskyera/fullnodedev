//! What an NVIDIA card can actually hold of the x16rs batch kernel, and what
//! that leaves worth sweeping.
//!
//! This module contains no CUDA and opens no device. It is the arithmetic that
//! turns four numbers the CUDA build reports about the kernel into the two ends
//! of a search space, so that the NVIDIA grid and the NVIDIA presets can be
//! derived and TESTED on a machine with no NVIDIA card in it.
//!
//! # The four numbers, and what they imply
//!
//! `cudaFuncGetAttributes` on the batch kernel, from the CUDA build on a Tesla
//! T4 (sm_75):
//!
//!   numRegs = 255, staticShared = 33984 B, maxThreadsPerBlock = 256,
//!   localPerThread = 792 B
//!
//! The block size is not a tuning axis: `block_miner.cu` declares
//! `__shared__ unsigned int local_nonces[256]` and reduces over a power-of-two
//! tree across the block, so 256 threads (8 warps) is the only size it is
//! correct at. Put those against one Turing multiprocessor's budgets:
//!
//!   * **Registers.** sm_75 has 65536 registers per SM, allocated per warp in
//!     units of 256. One warp of this kernel takes
//!     `ceil(255 * 32 / 256) * 256 = 8192` registers, so a block of 8 warps
//!     takes 65536: the ENTIRE register file, with nothing left over. Blocks per
//!     SM by registers = 1.
//!   * **Shared memory.** 64 KiB per SM against 33984 B a block. Two blocks
//!     would need 66.4 KiB. Blocks per SM by shared memory = 1.
//!   * **Threads.** 1024 per SM against 256 a block would allow 4.
//!   * **Blocks.** sm_75 allows 16 resident blocks per SM.
//!
//! The binding limit is 1, and it is binding twice over. So one multiprocessor
//! holds exactly ONE block of this kernel: 8 warps of the 32 the SM can track,
//! which is 25% occupancy. That is not a T4 quirk. Every NVIDIA architecture
//! from Volta to Blackwell has 65536 registers per SM (see [`SM_BUDGETS`]), and
//! 255 registers on 256 threads consumes all of them on every one of them, so
//! [`residency`] returns one block per SM for every entry in that table. The
//! measured confirmation is in `x16rs-cuda`: the runtime's own
//! `cudaOccupancyMaxActiveBlocksPerMultiprocessor` reported
//! `blocks_per_multiprocessor = 1` on the real T4.
//!
//! Three consequences, and they are the whole shape of the NVIDIA search space:
//!
//!   1. **The work-group floor is the multiprocessor count.** Below it, SMs have
//!      no work at all and the hashrate describes the launch being too small.
//!      `CudaDeviceLimits::work_groups_that_fill_the_card` is exactly this.
//!   2. **Above that floor, work_groups is a WAVE COUNT and nothing else.** With
//!      one resident block per SM there is no second block to overlap with; a
//!      launch of `k * sm_count` blocks runs k waves back to back. It does not
//!      add concurrency. All it adds is batch length, and the only thing longer
//!      batches buy is amortising the launch: one wave at unit_size 64 on a T4
//!      is about 87 ms of work against a launch overhead of a few microseconds,
//!      so that is already paid off at ONE wave. See [`wave_ceiling`] for what
//!      bounds it from above, which is latency rather than throughput.
//!   3. **unit_size is the only axis that changes what a resident block does.**
//!      At 25% occupancy each warp has to cover its own latency, and the card
//!      may or may not have the power headroom to let it. Which way that goes is
//!      not derivable, it is measurable, and the two cards measured go OPPOSITE
//!      ways: an RX 9070 XT wants 192 (latency bound, underfed), a Tesla T4
//!      wants 64 or less (66 to 67 W against a 70 W cap, so a bigger batch buys
//!      nothing the power limit will allow). Which is why the honest NVIDIA
//!      unit_size default is the smallest value anyone has MEASURED to win, and
//!      why the tuner's axis has to start below it.
//!
//! # What is derived here and what is not
//!
//! Derived: the residency, the work-group floor, the wave ceiling, and the fact
//! that the wave ceiling is card-size independent. Measured: the per-SM rate and
//! the unit_size ordering, both from one T4, both quoted with their source.
//! Chosen: where on the ladder between the floor and the ceiling each preset
//! tier sits. [`PRESET_LADDER`] says which is which, one line per number.

/// One NVIDIA multiprocessor's budgets, per compute capability.
///
/// From the CUDA C Programming Guide's "Technical Specifications per Compute
/// Capability" table. Only the fields this kernel's residency depends on are
/// carried; a field that never binds for a 256-thread block is still here
/// because leaving it out would hide WHY it never binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmBudget {
    pub compute_major: u32,
    pub compute_minor: u32,
    /// 32-bit registers in one multiprocessor's register file.
    pub registers_per_sm: u32,
    /// Registers are allocated a warp at a time, rounded up to this many.
    pub register_alloc_unit: u32,
    /// Shared memory one multiprocessor can hand out to resident blocks.
    pub shared_bytes_per_sm: u32,
    pub max_threads_per_sm: u32,
    pub max_warps_per_sm: u32,
    pub max_blocks_per_sm: u32,
}

/// Every NVIDIA architecture this miner can meet, oldest first.
///
/// The column that decides everything is `registers_per_sm`, and the striking
/// thing about it is that it has not moved since Volta: 65536 on all of them.
/// A kernel at 255 registers a thread on a 256-thread block wants exactly 65536,
/// so ONE block per SM is not a property of the T4 that happened to be measured.
/// It is a property of this kernel on NVIDIA hardware, full stop.
pub const SM_BUDGETS: [SmBudget; 8] = [
    // Volta, Titan V / V100.
    SmBudget {
        compute_major: 7,
        compute_minor: 0,
        registers_per_sm: 65_536,
        register_alloc_unit: 256,
        shared_bytes_per_sm: 98_304,
        max_threads_per_sm: 2_048,
        max_warps_per_sm: 64,
        max_blocks_per_sm: 32,
    },
    // Turing, T4 / RTX 20. The card the kernel attributes above came from.
    SmBudget {
        compute_major: 7,
        compute_minor: 5,
        registers_per_sm: 65_536,
        register_alloc_unit: 256,
        shared_bytes_per_sm: 65_536,
        max_threads_per_sm: 1_024,
        max_warps_per_sm: 32,
        max_blocks_per_sm: 16,
    },
    // Ampere GA100, A100.
    SmBudget {
        compute_major: 8,
        compute_minor: 0,
        registers_per_sm: 65_536,
        register_alloc_unit: 256,
        shared_bytes_per_sm: 167_936,
        max_threads_per_sm: 2_048,
        max_warps_per_sm: 64,
        max_blocks_per_sm: 32,
    },
    // Ampere GA10x, RTX 30.
    SmBudget {
        compute_major: 8,
        compute_minor: 6,
        registers_per_sm: 65_536,
        register_alloc_unit: 256,
        shared_bytes_per_sm: 102_400,
        max_threads_per_sm: 1_536,
        max_warps_per_sm: 48,
        max_blocks_per_sm: 16,
    },
    // Ada Lovelace, RTX 40.
    SmBudget {
        compute_major: 8,
        compute_minor: 9,
        registers_per_sm: 65_536,
        register_alloc_unit: 256,
        shared_bytes_per_sm: 102_400,
        max_threads_per_sm: 1_536,
        max_warps_per_sm: 48,
        max_blocks_per_sm: 24,
    },
    // Hopper, H100.
    SmBudget {
        compute_major: 9,
        compute_minor: 0,
        registers_per_sm: 65_536,
        register_alloc_unit: 256,
        shared_bytes_per_sm: 233_472,
        max_threads_per_sm: 2_048,
        max_warps_per_sm: 64,
        max_blocks_per_sm: 32,
    },
    // Blackwell datacentre, B100/B200.
    SmBudget {
        compute_major: 10,
        compute_minor: 0,
        registers_per_sm: 65_536,
        register_alloc_unit: 256,
        shared_bytes_per_sm: 233_472,
        max_threads_per_sm: 2_048,
        max_warps_per_sm: 64,
        max_blocks_per_sm: 32,
    },
    // Blackwell consumer, RTX 50.
    SmBudget {
        compute_major: 12,
        compute_minor: 0,
        registers_per_sm: 65_536,
        register_alloc_unit: 256,
        shared_bytes_per_sm: 102_400,
        max_threads_per_sm: 1_536,
        max_warps_per_sm: 48,
        max_blocks_per_sm: 24,
    },
];

/// The budgets for a compute capability, or the closest older one this table
/// knows.
///
/// A card newer than the table is treated as the newest entry rather than
/// refused: every entry agrees on the register file, which is the limit that
/// binds, so the answer for an unknown NVIDIA card is the same answer. A card
/// OLDER than the oldest entry falls back to the oldest, and the note in
/// [`Residency::limited_by`] says which entry answered.
pub fn sm_budget(compute_major: u32, compute_minor: u32) -> SmBudget {
    let key = (compute_major, compute_minor);
    let mut best = SM_BUDGETS[0];
    for budget in SM_BUDGETS {
        if (budget.compute_major, budget.compute_minor) <= key {
            best = budget;
        }
    }
    best
}

/// The kernel's own resource use, as `cudaFuncGetAttributes` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelResidency {
    pub regs_per_thread: u32,
    pub static_shared_bytes: u32,
    pub threads_per_block: u32,
    pub warp_size: u32,
}

/// `x16rs_cuda_main`, measured on the CUDA build.
///
/// Not a guess and not a target: these are the numbers the CUDA build printed
/// for its own kernel on a real Tesla T4, and they are quoted in
/// `CudaDeviceLimits::describe` so an operator's log can be checked against
/// them. `localPerThread = 792 B` is spill, and it is not in this struct because
/// local memory is backed by device DRAM and does not bound residency; it
/// bounds bandwidth, which is a different argument.
pub const X16RS_BATCH_KERNEL: KernelResidency = KernelResidency {
    regs_per_thread: 255,
    static_shared_bytes: 33_984,
    threads_per_block: 256,
    warp_size: 32,
};

/// What one multiprocessor holds, and which budget said so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Residency {
    pub blocks_per_sm: u32,
    pub warps_per_sm: u32,
    /// Warps resident against warps the SM can track, in percent. This kernel
    /// is register bound, so it is low by construction and cannot be raised
    /// from the host side by any launch shape.
    pub occupancy_percent_x10: u32,
    /// Registers one block consumes, after per-warp rounding.
    pub registers_per_block: u32,
    /// Which of the four budgets produced `blocks_per_sm`. When several tie,
    /// the tightest ones are named together, because "shared memory would have
    /// allowed 3" and "shared memory also allows exactly 1" are different facts
    /// about how much slack a kernel edit has.
    pub limited_by: &'static str,
}

/// Registers one warp of `kernel` consumes, after the hardware's per-warp
/// rounding.
pub fn registers_per_warp(kernel: KernelResidency) -> u32 {
    let unit = kernel.register_alloc_unit_or_default();
    let raw = kernel.regs_per_thread.max(1) * kernel.warp_size.max(1);
    raw.div_ceil(unit) * unit
}

impl KernelResidency {
    fn register_alloc_unit_or_default(self) -> u32 {
        // Every entry in SM_BUDGETS agrees on 256, and a kernel struct has no
        // architecture of its own; `residency` passes the real one in.
        256
    }

    pub fn warps_per_block(self) -> u32 {
        self.threads_per_block.max(1).div_ceil(self.warp_size.max(1))
    }
}

/// How many blocks of `kernel` one `budget` multiprocessor holds at once.
///
/// This is `cudaOccupancyMaxActiveBlocksPerMultiprocessor` done on the host with
/// no CUDA, so that the search space it implies can be tested without a card.
/// It is checked against the runtime's own answer on the one NVIDIA GPU this
/// project has run on, in
/// `the_host_side_occupancy_agrees_with_what_the_t4_runtime_reported`.
pub fn residency(kernel: KernelResidency, budget: SmBudget) -> Residency {
    let warps = kernel.warps_per_block();
    let per_warp = kernel.regs_per_thread.max(1) * kernel.warp_size.max(1);
    let unit = budget.register_alloc_unit.max(1);
    let regs_per_block = per_warp.div_ceil(unit) * unit * warps;

    let by_registers = budget.registers_per_sm / regs_per_block.max(1);
    let by_shared = if kernel.static_shared_bytes == 0 {
        u32::MAX
    } else {
        budget.shared_bytes_per_sm / kernel.static_shared_bytes
    };
    let by_threads = budget.max_threads_per_sm / kernel.threads_per_block.max(1);
    let by_blocks = budget.max_blocks_per_sm;

    let blocks = by_registers.min(by_shared).min(by_threads).min(by_blocks);
    let limited_by = match (
        by_registers == blocks,
        by_shared == blocks,
        by_threads == blocks,
    ) {
        (true, true, _) => "registers and shared memory, both exactly",
        (true, false, _) => "registers",
        (false, true, _) => "shared memory",
        (false, false, true) => "threads per multiprocessor",
        (false, false, false) => "resident blocks per multiprocessor",
    };

    let resident_warps = blocks * warps;
    Residency {
        blocks_per_sm: blocks,
        warps_per_sm: resident_warps,
        occupancy_percent_x10: (resident_warps * 1_000) / budget.max_warps_per_sm.max(1),
        registers_per_block: regs_per_block,
        limited_by,
    }
}

// ---------------------------------------------------------------------------
// From residency to a search space
// ---------------------------------------------------------------------------

/// The only block size `block_miner.cu` is correct at.
pub const CUDA_LOCAL_SIZE: u32 = 256;

/// The smallest launch that leaves no multiprocessor idle.
///
/// This is the work-group axis's floor, and it is not a preference. A launch of
/// fewer blocks than the card has multiprocessors leaves some of them with
/// nothing to do, so its hashrate is a statement about the launch being too
/// small rather than about the shape.
pub fn work_group_floor(sm_count: u32, blocks_per_sm: u32) -> u32 {
    sm_count.max(1).saturating_mul(blocks_per_sm.max(1)).max(1)
}

/// How many times over a launch fills the card.
pub fn waves(work_groups: u32, sm_count: u32, blocks_per_sm: u32) -> f64 {
    work_groups as f64 / work_group_floor(sm_count, blocks_per_sm) as f64
}

/// The p95 batch-latency ceiling the tuner scores against, in milliseconds.
///
/// Kept here as its own constant because `autotune16` is compiled only with a
/// GPU backend or under `cfg(test)`, and this module has to answer without
/// either. `the_two_copies_of_the_latency_ceiling_are_one_number` asserts they
/// agree, so a change to one fails rather than drifts.
pub const P95_BATCH_CEILING_MS: f64 = 1_500.0;

/// The most waves whose batch still comes in under the latency ceiling.
///
/// A batch is atomic: a template change cannot take effect until the launch
/// returns, so a long batch throws away work and delays every job switch. The
/// tuner refuses any shape whose p95 batch exceeds `P95_BATCH_CEILING_MS`, which
/// makes this the real ceiling on the work-group axis. Memory is not: a T4 holds
/// about 7400 work groups at unit_size 128 and the latency ceiling allows about
/// 344 of them.
///
/// # Why the answer is a wave count and not a work-group count
///
/// A batch of `w` waves is `w * sm_count * local_size * unit_size` nonces, and a
/// card with `sm_count` multiprocessors hashes at about `sm_count * r` where `r`
/// is the per-SM rate. The `sm_count` cancels:
///
///   `w <= ceiling_seconds * r / (local_size * unit_size)`
///
/// So the ceiling is the SAME number of waves on a 40-SM T4 and a 170-SM 5090,
/// as long as the per-SM rate is comparable, which for a kernel pinned at one
/// block and 8 warps per SM it broadly is. That is what makes a card-agnostic
/// preset table possible at all on this vendor: the tier can be a wave count.
pub fn wave_ceiling(unit_size: u32, local_size: u32, hashes_per_second_per_sm: f64) -> f64 {
    let per_wave = (local_size.max(1) as f64) * (unit_size.max(1) as f64);
    if per_wave <= 0.0 || !hashes_per_second_per_sm.is_finite() {
        return 0.0;
    }
    (P95_BATCH_CEILING_MS / 1000.0) * hashes_per_second_per_sm / per_wave
}

// ---------------------------------------------------------------------------
// The one NVIDIA measurement there is
// ---------------------------------------------------------------------------

/// Multiprocessors on the Tesla T4 the kernel was measured on.
pub const MEASURED_T4_SM_COUNT: u32 = 40;

/// The T4's steady-state rate at its best measured shape.
///
/// Colab Tesla T4, repeat 16, fixed corpus, steady state after 40 warm-up
/// batches, flat to 0.57%, at work_groups 256, local_size 256, unit_size 64.
/// The other two points measured in the same run were 7.19 MH/s at unit_size 96
/// and 7.06 at 128. `nvidia-smi` during the run: 66 to 67 W against a 70 W cap,
/// 63 to 74 C, SM clock 1140 to 1305 MHz. The card was POWER capped.
pub const MEASURED_T4_HPS: f64 = 7.54e6;

/// The best unit_size anyone has measured on an NVIDIA card.
///
/// It is the SMALLEST of the three that were measured and the trend was still
/// falling, so the real optimum may well be below it. That is the reason the
/// tuner's unit_size axis starts at 32 and the reason no preset here names a
/// value above this one.
pub const MEASURED_T4_BEST_UNIT_SIZE: u32 = 64;

/// The T4's rate per multiprocessor, which is the quantity [`wave_ceiling`]
/// needs and the only one that transfers between cards of different sizes.
///
/// 7.54 MH/s over 40 multiprocessors is 188.5 kH/s each. On a card whose clocks
/// are not being held down by a 70 W cap this is pessimistic, and pessimistic is
/// the safe direction: it makes the wave ceiling SMALLER, so a preset derived
/// from it has shorter batches than it strictly needs rather than longer.
pub fn measured_hashes_per_second_per_sm() -> f64 {
    MEASURED_T4_HPS / MEASURED_T4_SM_COUNT as f64
}

// ---------------------------------------------------------------------------
// The presets
// ---------------------------------------------------------------------------

/// One preset tier: what the miner runs on an NVIDIA card BEFORE anybody tunes
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NvidiaPreset {
    pub tier: i8,
    pub profile: &'static str,
    pub work_groups: u32,
    pub unit_size: u32,
}

/// The NVIDIA preset ladder, and exactly how much of it is measurement.
///
/// # THESE ARE NOT MEASURED SHAPES. Read this before quoting one.
///
/// Nobody has run a hashrate sweep on any of these cards. What follows is the
/// most defensible starting point the hardware and the one T4 measurement
/// allow, and it exists to be REPLACED by `x16rs_gate`/autotune on the operator's
/// own card. The panel's Auto Tune button is what turns these into numbers.
///
/// **unit_size 64 on every tier: the one measured value, not a spread.**
/// The T4 measured 64 -> 7.54 MH/s, 96 -> 7.19, 128 -> 7.06. Every tier in the
/// table this replaces named 96 or 128, i.e. every NVIDIA operator shipped with
/// a value the only NVIDIA measurement ranks LAST or second to last. Spreading
/// unit_size across the tiers was the invention being removed: with exactly one
/// resident block per SM (see [`residency`]) unit_size does not trade power for
/// throughput in a way anything here can predict, and the two cards measured
/// disagree about its direction. So it is pinned to the one value that won, and
/// the tuner's axis runs 32/48/64/96/128 so a card that wants less can find it.
///
/// **work_groups: a wave count, bounded by the latency ceiling.**
/// Derived: one resident block per SM makes work_groups a wave count, and
/// [`wave_ceiling`] at unit_size 64 on the measured per-SM rate is 17.2 waves.
/// Chosen: where each tier sits between 1 wave and that ceiling. The numbers
/// below are multiples of 64 because `gpu_arch::tune_workgroups` rounds to 64
/// and clamps to at least 256, and they are checked in
/// `every_nvidia_preset_lands_inside_the_derived_bracket` against the real
/// multiprocessor counts of every NVIDIA card in `PANEL_GPU_PRESETS`.
///
/// What the ladder is NOT is a claim that a higher tier is faster. On the one
/// NVIDIA card measured it would not be: the card sat on its power limit, where
/// a longer batch buys nothing. The ladder is an aggressiveness dial whose top
/// end stays inside the latency ceiling, which is a much weaker claim, and the
/// weaker claim is the true one.
pub const PRESET_LADDER: [NvidiaPreset; 5] = [
    NvidiaPreset {
        tier: 0,
        profile: "nvidia_eco",
        work_groups: 256,
        unit_size: 64,
    },
    NvidiaPreset {
        tier: 1,
        profile: "nvidia_balanced",
        work_groups: 320,
        unit_size: 64,
    },
    NvidiaPreset {
        tier: 2,
        profile: "nvidia_profit",
        work_groups: 384,
        unit_size: 64,
    },
    NvidiaPreset {
        tier: 3,
        profile: "nvidia_performance",
        work_groups: 512,
        unit_size: 64,
    },
    NvidiaPreset {
        tier: 4,
        profile: "nvidia_max",
        work_groups: 768,
        unit_size: 64,
    },
];

/// The shape a named NVIDIA profile starts at, or `None` if it is not one.
pub fn preset_tuning(profile: &str) -> Option<(u32, u32)> {
    PRESET_LADDER
        .iter()
        .find(|preset| preset.profile == profile)
        .map(|preset| (preset.work_groups, preset.unit_size))
}

/// Multiprocessor counts of the NVIDIA cards the panel offers, so the preset
/// ladder can be checked in WAVES rather than in work groups.
///
/// Published die configurations, one entry per panel slug in
/// `gpu_arch::PANEL_GPU_PRESETS`. They are here rather than in the panel because
/// nothing the panel does needs them: this is the denominator that turns a
/// work-group count into the only unit that means anything on this vendor.
pub const NVIDIA_PANEL_SM_COUNTS: [(&str, u32); 9] = [
    ("rtx3060", 28),
    ("rtx4060", 24),
    ("rtx3070", 46),
    ("rtx4070", 46),
    ("rtx4090", 128),
    ("rtx5060", 30),
    ("rtx5070", 48),
    ("rtx5080", 84),
    ("rtx5090", 170),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The four budgets, worked through by hand for the one card that was
    /// measured, against the answer the CUDA runtime gave on it.
    ///
    /// `x16rs-cuda`'s own T4 fixture records
    /// `blocks_per_multiprocessor: 1` straight from
    /// `cudaOccupancyMaxActiveBlocksPerMultiprocessor`. This module derives that
    /// number from the published SM budgets instead. If the two ever disagree,
    /// one of them is wrong and the search space is built on it.
    #[test]
    fn the_host_side_occupancy_agrees_with_what_the_t4_runtime_reported() {
        let turing = sm_budget(7, 5);
        let r = residency(X16RS_BATCH_KERNEL, turing);

        // 255 * 32 = 8160, rounded up to a multiple of 256 is 8192, times 8
        // warps is 65536: the entire register file of a Turing SM.
        assert_eq!(registers_per_warp(X16RS_BATCH_KERNEL), 8_192);
        assert_eq!(r.registers_per_block, 65_536);
        assert_eq!(r.registers_per_block, turing.registers_per_sm);

        // And shared memory says one as well, independently: 2 * 33984 is
        // 67968, which is over the 65536 a Turing SM hands out.
        assert!(2 * X16RS_BATCH_KERNEL.static_shared_bytes > turing.shared_bytes_per_sm);

        assert_eq!(r.blocks_per_sm, 1, "the CUDA runtime reported 1 on the T4");
        assert_eq!(r.limited_by, "registers and shared memory, both exactly");
        assert_eq!(r.warps_per_sm, 8);
        // 8 of 32 warps: 25.0%.
        assert_eq!(r.occupancy_percent_x10, 250);
    }

    /// One block per SM on every NVIDIA architecture, not just the one measured.
    ///
    /// This is the load-bearing generalisation. If it failed on some
    /// architecture, that card's work-group floor would be a multiple of its SM
    /// count and every wave figure in this module would be wrong for it.
    #[test]
    fn every_nvidia_architecture_holds_exactly_one_block_of_this_kernel() {
        for budget in SM_BUDGETS {
            let r = residency(X16RS_BATCH_KERNEL, budget);
            assert_eq!(
                r.blocks_per_sm,
                1,
                "sm_{}{} holds {} blocks, so the wave arithmetic does not apply to it",
                budget.compute_major,
                budget.compute_minor,
                r.blocks_per_sm
            );
            // Register bound on all of them, because the register file has been
            // 65536 since Volta and this kernel wants all of it.
            assert_eq!(budget.registers_per_sm, 65_536);
            assert!(
                r.limited_by.contains("registers"),
                "sm_{}{} is limited by {} instead",
                budget.compute_major,
                budget.compute_minor,
                r.limited_by
            );
            // Occupancy never exceeds a third, and cannot be raised by any
            // launch shape: it is decided before the host picks one.
            assert!(
                r.occupancy_percent_x10 <= 250,
                "sm_{}{} at {:.1}% occupancy",
                budget.compute_major,
                budget.compute_minor,
                r.occupancy_percent_x10 as f64 / 10.0
            );
        }
    }

    /// A kernel that used fewer registers really would fit more blocks, so the
    /// test above is measuring the kernel and not a constant `1`.
    #[test]
    fn the_residency_arithmetic_responds_to_the_kernel_it_is_given() {
        let turing = sm_budget(7, 5);
        let lean = KernelResidency {
            regs_per_thread: 32,
            static_shared_bytes: 1_024,
            threads_per_block: 256,
            warp_size: 32,
        };
        let r = residency(lean, turing);
        // 32 * 32 = 1024 a warp, 8192 a block, so registers allow 8; threads
        // allow 4; blocks allow 16. Threads bind.
        assert_eq!(r.blocks_per_sm, 4);
        assert_eq!(r.limited_by, "threads per multiprocessor");
        assert_eq!(r.warps_per_sm, 32);
        assert_eq!(r.occupancy_percent_x10, 1_000);

        // And halving the register count of the real kernel is exactly what it
        // would take to hold two blocks, which is the slack a kernel edit has:
        // none.
        let halved = KernelResidency {
            regs_per_thread: 127,
            static_shared_bytes: 32_768,
            ..X16RS_BATCH_KERNEL
        };
        assert_eq!(residency(halved, turing).blocks_per_sm, 2);
    }

    /// The wave ceiling is the same number of waves whatever size the card is.
    ///
    /// This is the property that lets one preset table serve a 24-SM RTX 4060
    /// and a 170-SM RTX 5090 without naming either.
    #[test]
    fn the_latency_wave_ceiling_does_not_depend_on_how_big_the_card_is() {
        let per_sm = measured_hashes_per_second_per_sm();
        let ceiling = wave_ceiling(MEASURED_T4_BEST_UNIT_SIZE, CUDA_LOCAL_SIZE, per_sm);
        assert!(
            (ceiling - 17.2).abs() < 0.1,
            "17.2 waves at unit_size 64 on the measured per-SM rate, got {ceiling:.2}"
        );

        // Derived the long way round on three card sizes: turn the wave ceiling
        // into work groups, into nonces, into seconds, and check the batch
        // really does land on the latency ceiling.
        for sm_count in [24u32, 40, 128, 170] {
            let work_groups = (ceiling * sm_count as f64) as u32;
            let nonces =
                work_groups as u64 * CUDA_LOCAL_SIZE as u64 * MEASURED_T4_BEST_UNIT_SIZE as u64;
            let card_hps = per_sm * sm_count as f64;
            let batch_ms = nonces as f64 / card_hps * 1000.0;
            assert!(
                (batch_ms - P95_BATCH_CEILING_MS).abs() < 20.0,
                "{sm_count} SMs: {work_groups} work groups is a {batch_ms:.0} ms batch"
            );
        }

        // A bigger unit_size buys fewer waves, in exact proportion: the batch is
        // the product, so doubling one halves the other.
        let at_128 = wave_ceiling(128, CUDA_LOCAL_SIZE, per_sm);
        assert!((at_128 * 2.0 - ceiling).abs() < 1e-6);
        assert!(at_128 > 8.0 && at_128 < 9.0, "{at_128:.2} waves at 128");
    }

    /// One wave already amortises the launch, so waves beyond a handful buy
    /// throughput that rounds to nothing and cost latency that does not.
    ///
    /// This is the argument for the preset ladder being short rather than the
    /// 512..3584 it replaces.
    #[test]
    fn one_wave_is_already_long_enough_to_hide_a_kernel_launch() {
        let per_sm = measured_hashes_per_second_per_sm();
        // One wave on a T4 at the measured optimum.
        let nonces =
            MEASURED_T4_SM_COUNT as u64 * CUDA_LOCAL_SIZE as u64 * MEASURED_T4_BEST_UNIT_SIZE as u64;
        let seconds = nonces as f64 / (per_sm * MEASURED_T4_SM_COUNT as f64);
        assert!(
            (seconds - 0.0869).abs() < 0.001,
            "one T4 wave is {seconds:.4}s"
        );
        // A CUDA kernel launch is single-digit microseconds. Against 87 ms that
        // is under a tenth of a percent, so the whole throughput case for a long
        // batch is already spent at one wave.
        let launch_overhead_seconds = 10e-6;
        assert!(launch_overhead_seconds / seconds < 0.001);
    }

    /// Every preset lands between one wave and the latency ceiling, on every
    /// NVIDIA card the panel offers.
    ///
    /// Table driven on purpose: this is the test that a future edit to the
    /// ladder cannot get past without either staying inside the bracket or
    /// changing the bracket and saying why.
    #[test]
    fn every_nvidia_preset_lands_inside_the_derived_bracket() {
        let per_sm = measured_hashes_per_second_per_sm();
        let mut rows = Vec::new();

        for preset in PRESET_LADDER {
            // The whole ladder is at the one measured unit_size, and no tier is
            // allowed above it: an unmeasured guess that is SLOWER than the one
            // measurement is the defect this replaces.
            assert!(
                preset.unit_size <= MEASURED_T4_BEST_UNIT_SIZE,
                "{} names unit_size {}, above the only measured NVIDIA optimum ({})",
                preset.profile,
                preset.unit_size,
                MEASURED_T4_BEST_UNIT_SIZE
            );
            // And on the tuner's grid, or the tune could never return to it.
            assert!(
                [32u32, 48, 64, 96, 128].contains(&preset.unit_size),
                "{} names unit_size {}, which is not a grid point",
                preset.profile,
                preset.unit_size
            );
            // `gpu_arch::tune_workgroups` rounds to a multiple of 64 and clamps
            // to at least 256, so a preset outside that is not the shape the
            // card is given.
            assert!(
                preset.work_groups >= 256 && preset.work_groups % 64 == 0,
                "{} names {} work groups, which tune_workgroups would rewrite",
                preset.profile,
                preset.work_groups
            );

            let ceiling = wave_ceiling(preset.unit_size, CUDA_LOCAL_SIZE, per_sm);
            for (slug, sm_count) in NVIDIA_PANEL_SM_COUNTS {
                // A card only ever sees the tiers its own limits allow.
                if preset.tier > crate::gpu_arch::ArchLimits::panel_max_tier(slug) {
                    continue;
                }
                let w = waves(preset.work_groups, sm_count, 1);
                assert!(
                    w >= 1.0,
                    "{slug} ({sm_count} SMs) on {}: {w:.1} waves leaves multiprocessors idle",
                    preset.profile
                );
                assert!(
                    w <= ceiling,
                    "{slug} ({sm_count} SMs) on {}: {w:.1} waves is over the {ceiling:.1} the \
                     latency ceiling allows, so its batches exceed {P95_BATCH_CEILING_MS} ms",
                    preset.profile
                );
                rows.push((slug, preset.profile, sm_count, preset.work_groups, w));
            }
        }

        assert!(
            rows.len() >= 30,
            "only {} card/tier combinations were checked",
            rows.len()
        );
    }

    /// The table this replaces would not have passed the test above.
    ///
    /// Kept as a literal so the improvement is computed rather than asserted,
    /// and so nobody reintroduces it believing it was fine.
    #[test]
    fn the_shipped_nvidia_table_was_outside_the_bracket_and_below_the_measurement() {
        let shipped: [(&str, u32, u32); 5] = [
            ("nvidia_eco", 512, 128),
            ("nvidia_balanced", 1024, 128),
            ("nvidia_profit", 1280, 96),
            ("nvidia_performance", 1792, 96),
            ("nvidia_max", 3584, 128),
        ];
        let per_sm = measured_hashes_per_second_per_sm();

        for (profile, _, unit_size) in shipped {
            assert!(
                unit_size > MEASURED_T4_BEST_UNIT_SIZE,
                "{profile} was supposed to be above the measured optimum"
            );
        }

        // On the T4 itself, every shipped tier was over the latency ceiling.
        let mut over = 0;
        for (profile, work_groups, unit_size) in shipped {
            let ceiling = wave_ceiling(unit_size, CUDA_LOCAL_SIZE, per_sm);
            let w = waves(work_groups, MEASURED_T4_SM_COUNT, 1);
            if w > ceiling {
                over += 1;
            }
            let nonces = work_groups as u64 * CUDA_LOCAL_SIZE as u64 * unit_size as u64;
            let batch_s = nonces as f64 / MEASURED_T4_HPS;
            assert!(
                batch_s > P95_BATCH_CEILING_MS / 1000.0,
                "{profile} was a {batch_s:.2}s batch on a T4, which the tuner would have accepted"
            );
        }
        assert_eq!(over, 5, "every shipped tier was over the wave ceiling");

        // The worst of them, spelled out: nvidia_max is 3584 x 256 x 128 =
        // 117 M nonces, 15.6 s a batch on a T4, ten times the ceiling.
        let max_batch = 3584u64 * 256 * 128;
        assert!((max_batch as f64 / MEASURED_T4_HPS - 15.58).abs() < 0.1);
    }

    /// `sm_budget` answers for cards this table has never heard of, and the
    /// answer is the same one, because the register file is the same.
    #[test]
    fn an_unknown_compute_capability_still_gets_one_block_per_sm() {
        for (major, minor) in [(6u32, 1u32), (7, 2), (8, 7), (9, 1), (13, 0), (99, 9)] {
            let r = residency(X16RS_BATCH_KERNEL, sm_budget(major, minor));
            assert_eq!(
                r.blocks_per_sm, 1,
                "sm_{major}{minor} fell through to a budget that says {}",
                r.blocks_per_sm
            );
        }
        // Exact matches come back exactly.
        assert_eq!(sm_budget(7, 5).max_warps_per_sm, 32);
        assert_eq!(sm_budget(8, 9).max_blocks_per_sm, 24);
        // A capability below the whole table gets the oldest entry rather than
        // a panic or a zero.
        assert_eq!(sm_budget(3, 5).compute_major, 7);
    }

    /// The tuner's ceiling and this module's copy of it are one number.
    #[test]
    fn the_two_copies_of_the_latency_ceiling_are_one_number() {
        assert_eq!(
            P95_BATCH_CEILING_MS,
            crate::autotune16::P95_BATCH_CEILING_MS
        );
        assert_eq!(CUDA_LOCAL_SIZE, 256);
    }

    /// The profile names here and the ones `efficiency` dispatches on are the
    /// same five, so a rename cannot leave a tier silently unmatched.
    #[test]
    fn the_ladder_covers_exactly_the_named_nvidia_profiles() {
        for tier in 0i8..=4 {
            let profile =
                crate::efficiency::tier_profile_for_vendor(crate::gpu_arch::GpuVendor::Nvidia, tier);
            let preset = PRESET_LADDER
                .iter()
                .find(|p| p.profile == profile)
                .unwrap_or_else(|| panic!("{profile} has no entry in the ladder"));
            assert_eq!(preset.tier, tier);
            assert_eq!(
                crate::efficiency::profile_tier(profile),
                tier,
                "{profile} disagrees about its own tier"
            );
            assert_eq!(
                crate::efficiency::profile_tuning(profile),
                (preset.work_groups, preset.unit_size),
                "{profile} in efficiency.rs is not the ladder entry"
            );
        }
        assert_eq!(preset_tuning("amd_max"), None);
        assert_eq!(preset_tuning("nvidia_max"), Some((768, 64)));
    }
}
