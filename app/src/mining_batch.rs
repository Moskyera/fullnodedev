//! Shared CPU/GPU batch merge helpers and block-miner backend trait.

#[cfg(any(feature = "ocl", feature = "cuda"))]
use std::sync::Arc;

use crate::hash_util::hash_more_power;

#[cfg(feature = "ocl")]
use crate::gpu_oom::GpuBatchError;
#[cfg(any(feature = "ocl", feature = "cuda"))]
use crate::mining_runtime::MiningRuntimeState;
#[cfg(feature = "ocl")]
use crate::opencl_gpu::OpenclGpuHandle;
#[cfg(feature = "ocl")]
use crate::opencl_gpu::block::do_group_block_mining_opencl_shares;

#[cfg(any(feature = "ocl", feature = "cuda", test))]
const GPU_ERROR_CPU_RECOVERY_NONCES: u32 = 100_000;

/// Block intro serialization is a fixed 89-byte layout, shared by the OpenCL
/// kernel (opencl_gpu/block.rs) and CUDA (`x16rs_cuda::STUFF_BYTES`). The nonce
/// lives at bytes 79..83, so a shorter or longer intro would be hashed over a
/// different message length than the kernel used and the resulting mismatch would
/// be charged to the card as a "GPU integrity error".
pub const BLOCK_INTRO_BYTES: usize = 89;

/// What a failed CUDA batch should do besides falling back to the CPU.
#[cfg(any(feature = "cuda", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CudaFailureAction {
    /// Halve the effective work groups. Skipped for a sticky context fault: that
    /// fault is grid independent and x16rs-cuda rebuilds the context itself, so
    /// shrinking the launch would only cost throughput once the card recovers.
    pub halve_workgroups: bool,
}

/// Decide what to do about a failed CUDA batch from the fault class.
///
/// Whether the card is parked is NOT decided here: both backends now defer to the
/// shared [`crate::gpu_oom::GpuQuarantine`], which needs the failures to persist in
/// count AND in time, and which re-probes instead of latching the card off.
#[cfg(any(feature = "cuda", test))]
pub fn cuda_failure_action(sticky: bool) -> CudaFailureAction {
    CudaFailureAction {
        halve_workgroups: !sticky,
    }
}

/// Inputs for one block-mining batch across CPU / CUDA / OpenCL backends.
pub struct BatchCtx {
    pub height: u64,
    pub block_intro: Vec<u8>,
    pub nonce_start: u32,
    pub nonce_space: u32,
    pub configured_wg: u32,
    pub localsize: u32,
    pub unitsize: u32,
    pub thermal_wg_cap: Option<u32>,
    /// Threshold for the pool share list, or `None` for solo mining.
    ///
    /// A pool serves its SHARE target as the template `target_hash`, so this is
    /// that same value and there is no second threshold to carry. `None` is what
    /// keeps solo mining byte identical: the OpenCL kernel is launched with
    /// share_capacity=0, skips the appending block entirely, and the host neither
    /// writes nor reads a share buffer.
    pub share_target: Option<[u8; 32]>,
}

/// One nonce whose hash beat the share target, already re-hashed on the CPU.
///
/// Against a pool each of these is a separately payable PPLNS share, which is
/// why a batch reports all of them instead of only its strongest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MinedShare {
    pub nonce: u32,
    pub hash: [u8; 32],
}

/// What a GPU backend produced for one batch, before the CPU tail is merged in.
pub struct GpuBatchOutcome {
    /// The single strongest nonce/hash. Solo mining and the block-found path
    /// read only this, and it is produced exactly as it always was.
    pub best: (u32, [u8; 32]),
    pub gpu_nonce_space: u32,
    /// Verified extra shares. Always empty for solo mining, and for CUDA, which
    /// still reports one result per batch.
    pub shares: Vec<MinedShare>,
    /// Hits the kernel counted but could not store. Non-zero means the fixed
    /// capacity was exceeded and the miner is being paid for less than it mined.
    pub share_overflow: u64,
}

impl GpuBatchOutcome {
    /// A best-only outcome, i.e. what every backend produced before the pool
    /// share list existed.
    pub fn best_only(best: (u32, [u8; 32]), gpu_nonce_space: u32) -> GpuBatchOutcome {
        GpuBatchOutcome {
            best,
            gpu_nonce_space,
            shares: Vec::new(),
            share_overflow: 0,
        }
    }
}

/// Result of one batch including GPU/CPU nonce accounting for stats.
pub struct BatchResult {
    pub head_nonce: u32,
    pub result_hash: [u8; 32],
    pub gpu_nonce_space: u32,
    pub cpu_nonce_space: u32,
    /// Payable nonces BESIDES `head_nonce`, which is submitted on its own. Empty
    /// for every solo batch and every CPU batch.
    pub shares: Vec<MinedShare>,
    /// Hits the GPU counted but could not store.
    pub share_overflow: u64,
}

impl BatchResult {
    /// Every submission this batch owes the upstream: the head result plus each
    /// further share. One entry per payable nonce, which is the whole point -
    /// reporting one per batch tied pool credit to batch cadence, not hashrate.
    pub fn submission_nonces(&self) -> Vec<u32> {
        let mut nonces = Vec::with_capacity(self.shares.len() + 1);
        nonces.push(self.head_nonce);
        nonces.extend(self.shares.iter().map(|share| share.nonce));
        nonces
    }
}

/// Split a kernel share readback into what was stored and what was lost.
///
/// `hits` is the kernel's atomic counter, which counts EVERY nonce under the
/// share target including those that did not fit. Returning the overflow instead
/// of silently truncating is what lets the miner tell its operator it is
/// undersampling rather than quietly losing income.
pub fn split_share_readback(hits: u64, capacity: usize) -> (usize, u64) {
    let capacity = capacity as u64;
    (hits.min(capacity) as usize, hits.saturating_sub(capacity))
}

pub struct GpuBatchPlan {
    pub workgroups_eff: u32,
    pub gpu_nonce_space: u32,
}

/// Plan how many work groups fit in the nonce window.
pub fn plan_gpu_batch(
    nonce_space: u32,
    wg_cap: u32,
    localsize: u32,
    unitsize: u32,
) -> Option<GpuBatchPlan> {
    let unit_batch = (localsize as u64).saturating_mul(unitsize as u64);
    if wg_cap == 0 || unit_batch == 0 {
        return None;
    }
    let workgroups_by_space = (nonce_space as u64 / unit_batch) as u32;
    let workgroups_eff = workgroups_by_space.min(wg_cap);
    if workgroups_eff == 0 {
        return None;
    }
    let gpu_nonce_space = workgroups_eff
        .saturating_mul(localsize)
        .saturating_mul(unitsize);
    Some(GpuBatchPlan {
        workgroups_eff,
        gpu_nonce_space,
    })
}

/// After a partial GPU batch, mine the remaining nonce range on CPU and keep the best hash.
pub fn merge_cpu_tail(
    gpu_best: (u32, [u8; 32]),
    height: u64,
    block_intro: Vec<u8>,
    tail_start: u32,
    tail_space: u32,
    cpu_mine: impl FnOnce(u64, Vec<u8>, u32, u32) -> (u32, [u8; 32]),
) -> (u32, [u8; 32]) {
    if tail_space == 0 {
        return gpu_best;
    }
    let cpu_tail = cpu_mine(height, block_intro, tail_start, tail_space);
    if hash_more_power(&cpu_tail.1, &gpu_best.1) {
        cpu_tail
    } else {
        gpu_best
    }
}

/// Verify one GPU nonce/hash pair before it can reach submission.
///
/// Used for the batch's best result AND for every entry of the pool share list:
/// a card returning garbage has to be caught here, not forwarded to the pool as
/// a hundred bad shares that get the miner throttled or banned.
pub fn verify_gpu_nonce_result(
    height: u64,
    block_intro: &[u8],
    nonce_start: u32,
    gpu_nonce_space: u32,
    best: &(u32, [u8; 32]),
) -> Result<(), String> {
    if gpu_nonce_space == 0 {
        return Err("GPU integrity check received an empty nonce range".to_string());
    }
    if best.0.wrapping_sub(nonce_start) >= gpu_nonce_space {
        return Err(format!(
            "GPU returned nonce {} outside batch starting at {} (size {})",
            best.0, nonce_start, gpu_nonce_space
        ));
    }
    if block_intro.len() != BLOCK_INTRO_BYTES {
        return Err(format!(
            "block intro must be {BLOCK_INTRO_BYTES} bytes for nonce verification, got {}",
            block_intro.len()
        ));
    }

    let mut verify_intro = block_intro.to_vec();
    verify_intro[79..83].copy_from_slice(&best.0.to_be_bytes());
    let expected = x16rs::block_hash(height, &verify_intro);
    if expected != best.1 {
        return Err(format!(
            "GPU nonce/hash mismatch at nonce {}: gpu={} cpu={}",
            best.0,
            hex::encode(best.1),
            hex::encode(expected)
        ));
    }
    Ok(())
}

/// Verify every entry of a GPU share list and reject the batch if any is wrong.
///
/// Two things are checked per entry: that the CPU reproduces the hash the card
/// reported for that nonce, and that the hash really does beat the share target
/// the kernel was told to filter on. A card that gets either wrong is faulty, and
/// the existing policy for a faulty card is to fail the whole batch rather than
/// pick out the entries that happen to look right.
pub fn verify_gpu_shares(
    height: u64,
    block_intro: &[u8],
    nonce_start: u32,
    gpu_nonce_space: u32,
    share_target: &[u8; 32],
    raw: &[(u32, [u8; 32])],
) -> Result<Vec<MinedShare>, String> {
    let mut shares = Vec::with_capacity(raw.len());
    for entry in raw {
        verify_gpu_nonce_result(height, block_intro, nonce_start, gpu_nonce_space, entry)?;
        // Equal-inclusive, the same test the node and the pool apply: a hash
        // landing exactly on target is payable.
        if hash_more_power(share_target, &entry.1) {
            return Err(format!(
                "GPU listed nonce {} as a share but its hash {} is above the share target {}",
                entry.0,
                hex::encode(entry.1),
                hex::encode(share_target)
            ));
        }
        shares.push(MinedShare {
            nonce: entry.0,
            hash: entry.1,
        });
    }
    Ok(shares)
}

/// Finish a GPU batch: merge CPU tail nonces into the best hash.
pub fn finish_gpu_batch(
    height: u64,
    block_intro: Vec<u8>,
    nonce_start: u32,
    nonce_space: u32,
    gpu: GpuBatchOutcome,
    cpu_mine: impl Fn(u64, Vec<u8>, u32, u32) -> (u32, [u8; 32]),
) -> BatchResult {
    let gpu_nonce_space = gpu.gpu_nonce_space;
    let tail_space = nonce_space.saturating_sub(gpu_nonce_space);
    let (head_nonce, result_hash) = if tail_space > 0 {
        let tail_start = nonce_start.saturating_add(gpu_nonce_space);
        merge_cpu_tail(
            gpu.best,
            height,
            block_intro,
            tail_start,
            tail_space,
            cpu_mine,
        )
    } else {
        gpu.best
    };
    // The head result is submitted on its own, so keeping it in the list too
    // would buy a second HTTP round trip and a `duplicate` answer for it.
    let mut shares = gpu.shares;
    shares.retain(|share| share.nonce != head_nonce);
    BatchResult {
        head_nonce,
        result_hash,
        gpu_nonce_space,
        cpu_nonce_space: tail_space,
        shares,
        share_overflow: gpu.share_overflow,
    }
}

/// CPU-only fallback for the full nonce window.
pub fn cpu_batch_fallback(
    height: u64,
    block_intro: Vec<u8>,
    nonce_start: u32,
    nonce_space: u32,
    cpu_mine: impl Fn(u64, Vec<u8>, u32, u32) -> (u32, [u8; 32]),
) -> BatchResult {
    let (head_nonce, result_hash) = cpu_mine(height, block_intro, nonce_start, nonce_space);
    BatchResult {
        head_nonce,
        result_hash,
        gpu_nonce_space: 0,
        cpu_nonce_space: nonce_space,
        shares: Vec::new(),
        share_overflow: 0,
    }
}

/// Recover a bounded prefix after an OpenCL failure and skip the rest of the failed window.
#[cfg(any(feature = "ocl", feature = "cuda", test))]
fn cpu_gpu_error_recovery(
    height: u64,
    block_intro: Vec<u8>,
    nonce_start: u32,
    nonce_space: u32,
    cpu_mine: impl Fn(u64, Vec<u8>, u32, u32) -> (u32, [u8; 32]),
) -> BatchResult {
    cpu_batch_fallback(
        height,
        block_intro,
        nonce_start,
        nonce_space.min(GPU_ERROR_CPU_RECOVERY_NONCES),
        cpu_mine,
    )
}

pub trait BlockMinerBackend {
    fn is_gpu(&self) -> bool;
    fn run_batch(
        &self,
        ctx: &BatchCtx,
        cpu_mine: &dyn Fn(u64, Vec<u8>, u32, u32) -> (u32, [u8; 32]),
    ) -> BatchResult;
}

pub struct CpuBlockBackend;

impl BlockMinerBackend for CpuBlockBackend {
    fn is_gpu(&self) -> bool {
        false
    }

    fn run_batch(
        &self,
        ctx: &BatchCtx,
        cpu_mine: &dyn Fn(u64, Vec<u8>, u32, u32) -> (u32, [u8; 32]),
    ) -> BatchResult {
        cpu_batch_fallback(
            ctx.height,
            ctx.block_intro.clone(),
            ctx.nonce_start,
            ctx.nonce_space,
            cpu_mine,
        )
    }
}

#[cfg(feature = "ocl")]
pub struct OpenclBlockBackend {
    pub gpu: Arc<OpenclGpuHandle>,
    pub oom_fallback: bool,
    pub runtime: Arc<MiningRuntimeState>,
}

#[cfg(feature = "ocl")]
impl BlockMinerBackend for OpenclBlockBackend {
    fn is_gpu(&self) -> bool {
        true
    }

    fn run_batch(
        &self,
        ctx: &BatchCtx,
        cpu_mine: &dyn Fn(u64, Vec<u8>, u32, u32) -> (u32, [u8; 32]),
    ) -> BatchResult {
        // Quarantined, not disabled: the card is parked for a growing interval and
        // re-probed automatically, so a driver reset or a thermal excursion no
        // longer costs the whole session's GPU income.
        if self.gpu.quarantine_blocks_batch() {
            return cpu_gpu_error_recovery(
                ctx.height,
                ctx.block_intro.clone(),
                ctx.nonce_start,
                ctx.nonce_space,
                cpu_mine,
            );
        }
        let wg_cap = self.gpu.workgroups(ctx.configured_wg, ctx.thermal_wg_cap);
        let Some(plan) = plan_gpu_batch(ctx.nonce_space, wg_cap, ctx.localsize, ctx.unitsize)
        else {
            return cpu_batch_fallback(
                ctx.height,
                ctx.block_intro.clone(),
                ctx.nonce_start,
                ctx.nonce_space,
                cpu_mine,
            );
        };

        let gpu_result = {
            let opencl = self.gpu.lock_resources();
            do_group_block_mining_opencl_shares(
                &opencl,
                ctx.height,
                ctx.block_intro.clone(),
                ctx.nonce_start,
                plan.workgroups_eff,
                ctx.localsize,
                ctx.unitsize,
                ctx.share_target.as_ref(),
            )
        };

        match gpu_result {
            Err(e) => {
                eprintln!("[efficiency] GPU batch failed: {}", e.display());
                self.gpu
                    .on_batch_error(e, self.oom_fallback, ctx.configured_wg, &self.runtime);
                cpu_gpu_error_recovery(
                    ctx.height,
                    ctx.block_intro.clone(),
                    ctx.nonce_start,
                    ctx.nonce_space,
                    cpu_mine,
                )
            }
            Ok(output) => {
                // The best result is verified exactly as before, and every share
                // in the list goes through the same check. Whichever fails, the
                // batch is charged to the card and none of it is submitted.
                let verified = verify_gpu_nonce_result(
                    ctx.height,
                    &ctx.block_intro,
                    ctx.nonce_start,
                    plan.gpu_nonce_space,
                    &output.best,
                )
                .and_then(|()| match ctx.share_target.as_ref() {
                    Some(target) => verify_gpu_shares(
                        ctx.height,
                        &ctx.block_intro,
                        ctx.nonce_start,
                        plan.gpu_nonce_space,
                        target,
                        &output.shares,
                    ),
                    None => Ok(Vec::new()),
                });
                let shares = match verified {
                    Ok(shares) => shares,
                    Err(message) => {
                        let integrity_error =
                            GpuBatchError::Other(format!("GPU integrity error: {message}"));
                        eprintln!("[OpenCL] {}", integrity_error.display());
                        self.gpu.on_batch_error(
                            integrity_error,
                            false,
                            ctx.configured_wg,
                            &self.runtime,
                        );
                        return cpu_gpu_error_recovery(
                            ctx.height,
                            ctx.block_intro.clone(),
                            ctx.nonce_start,
                            ctx.nonce_space,
                            cpu_mine,
                        );
                    }
                };
                let (_, share_overflow) =
                    split_share_readback(output.share_hits, crate::opencl_gpu::SHARE_LIST_CAPACITY);
                self.gpu.on_batch_success(ctx.configured_wg, &self.runtime);
                finish_gpu_batch(
                    ctx.height,
                    ctx.block_intro.clone(),
                    ctx.nonce_start,
                    ctx.nonce_space,
                    GpuBatchOutcome {
                        best: output.best,
                        gpu_nonce_space: plan.gpu_nonce_space,
                        shares,
                        share_overflow,
                    },
                    cpu_mine,
                )
            }
        }
    }
}

#[cfg(feature = "cuda")]
pub struct CudaBlockBackend {
    pub cuda: Arc<crate::poworker::CudaMiningResources>,
    pub configured_wg: u32,
    pub runtime: Arc<MiningRuntimeState>,
}

#[cfg(feature = "cuda")]
impl BlockMinerBackend for CudaBlockBackend {
    fn is_gpu(&self) -> bool {
        true
    }

    fn run_batch(
        &self,
        ctx: &BatchCtx,
        cpu_mine: &dyn Fn(u64, Vec<u8>, u32, u32) -> (u32, [u8; 32]),
    ) -> BatchResult {
        // Honor the OOM/error backoff AND the thermal governor's graded cap, so a
        // hot or memory-pressured NVIDIA card steps down smoothly instead of
        // running full tilt until the hard thermal pause.
        if self.cuda.quarantine_blocks_batch() {
            // The card is parked for a growing backoff (see the alert below) and
            // will be re-probed automatically. Keep the bounded CPU recovery so
            // the miner still makes progress without hammering a sick device.
            return cpu_gpu_error_recovery(
                ctx.height,
                ctx.block_intro.clone(),
                ctx.nonce_start,
                ctx.nonce_space,
                cpu_mine,
            );
        }
        let wg_cap = self
            .cuda
            .effective_wg()
            .min(self.configured_wg)
            .min(ctx.thermal_wg_cap.unwrap_or(u32::MAX))
            .max(1);
        let localsize = x16rs_cuda::DEFAULT_LOCAL_SIZE;
        let Some(plan) = plan_gpu_batch(ctx.nonce_space, wg_cap, localsize, self.cuda.unit_size)
        else {
            return cpu_batch_fallback(
                ctx.height,
                ctx.block_intro.clone(),
                ctx.nonce_start,
                ctx.nonce_space,
                cpu_mine,
            );
        };

        match crate::poworker::do_group_block_mining_cuda(
            &self.cuda,
            ctx.height,
            ctx.block_intro.clone(),
            ctx.nonce_start,
            plan.workgroups_eff,
        ) {
            Err(e) => {
                eprintln!("[CUDA] batch failed: {e}");
                self.runtime.record_gpu_error_event();
                let report = self.cuda.note_batch_failure();
                let action = cuda_failure_action(e.is_sticky());
                if action.halve_workgroups {
                    // Back off the effective work-groups so the next batch tries a
                    // smaller, likely-runnable size instead of failing forever.
                    let reduced = self.cuda.record_error();
                    eprintln!("[CUDA] reducing work_groups to {reduced} after error");
                }
                self.cuda
                    .announce_quarantine(&report, &format!("Last error: {e}"));
                // Cap CPU recovery like the OpenCL path so a CUDA error cannot
                // make one CPU thread grind the whole GPU-sized nonce window.
                cpu_gpu_error_recovery(
                    ctx.height,
                    ctx.block_intro.clone(),
                    ctx.nonce_start,
                    ctx.nonce_space,
                    cpu_mine,
                )
            }
            Ok(best) => {
                // Re-verify the GPU's best hash on the CPU, like OpenCL, so a
                // faulty card cannot make us submit a wrong solution.
                if let Err(message) = verify_gpu_nonce_result(
                    ctx.height,
                    &ctx.block_intro,
                    ctx.nonce_start,
                    plan.gpu_nonce_space,
                    &best,
                ) {
                    eprintln!("[CUDA] GPU integrity error: {message}");
                    self.runtime.record_gpu_error_event();
                    // A card returning hashes the CPU cannot reproduce is as dead
                    // as one that fails to launch, so it shares the same budget.
                    let report = self.cuda.note_batch_failure();
                    self.cuda.record_error();
                    self.cuda.announce_quarantine(
                        &report,
                        "The card is returning hashes the CPU cannot reproduce; check for an overclock, bad memory or a failing driver.",
                    );
                    return cpu_gpu_error_recovery(
                        ctx.height,
                        ctx.block_intro.clone(),
                        ctx.nonce_start,
                        ctx.nonce_space,
                        cpu_mine,
                    );
                }
                // Clean batch: ramp effective work-groups back toward the max.
                self.cuda.record_success();
                // Best only for now: the CUDA kernel has no share list yet, so
                // this backend keeps exactly the behaviour it has today.
                finish_gpu_batch(
                    ctx.height,
                    ctx.block_intro.clone(),
                    ctx.nonce_start,
                    ctx.nonce_space,
                    GpuBatchOutcome::best_only(best, plan.gpu_nonce_space),
                    cpu_mine,
                )
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_best_result_requires_the_cpu_hash_and_batch_nonce() {
        let height = 1u64;
        let block_intro = vec![0u8; 89];
        let nonce_start = 11u32;
        let result_nonce = nonce_start + 42;
        let mut verified_intro = block_intro.clone();
        verified_intro[79..83].copy_from_slice(&result_nonce.to_be_bytes());
        let result_hash = x16rs::block_hash(height, &verified_intro);
        let valid = (result_nonce, result_hash);

        verify_gpu_nonce_result(height, &block_intro, nonce_start, 256, &valid).unwrap();

        let mut bad_hash = result_hash;
        bad_hash[0] ^= 1;
        assert!(
            verify_gpu_nonce_result(
                height,
                &block_intro,
                nonce_start,
                256,
                &(result_nonce, bad_hash)
            )
            .is_err()
        );
        assert!(
            verify_gpu_nonce_result(
                height,
                &block_intro,
                nonce_start,
                256,
                &(nonce_start + 256, result_hash)
            )
            .is_err()
        );
    }

    #[test]
    fn a_sticky_cuda_fault_does_not_halve_the_grid() {
        // A sticky context fault is grid independent and x16rs-cuda rebuilds the
        // context itself, so halving the work groups would only cost throughput.
        assert!(!cuda_failure_action(true).halve_workgroups);
        assert!(cuda_failure_action(false).halve_workgroups);
    }

    #[test]
    fn a_run_of_failed_cuda_batches_quarantines_instead_of_killing_the_card() {
        use crate::gpu_oom::{
            GPU_QUARANTINE_BASE_BACKOFF, GPU_QUARANTINE_MIN_ELAPSED, GPU_QUARANTINE_MIN_FAILURES,
            GpuGate, GpuQuarantine,
        };
        use std::time::{Duration, Instant};

        // CUDA used to disable the card on a bare count, with no at-floor and no
        // elapsed-time precondition, and with no way back short of a restart.
        // Both backends now share this policy, so a fast burst is survivable and a
        // real fault is only ever a timed quarantine.
        let quarantine = GpuQuarantine::new();
        let start = Instant::now();
        for i in 0..GPU_QUARANTINE_MIN_FAILURES * 2 {
            let now = start + Duration::from_millis(300 * i as u64);
            assert!(
                quarantine.record_failure(now).quarantined.is_none(),
                "a burst of {} failures in under a minute must not park a CUDA card",
                i + 1
            );
        }

        let quarantine = GpuQuarantine::new();
        let mut armed = None;
        for i in 0..GPU_QUARANTINE_MIN_FAILURES {
            let now = start + Duration::from_secs(10) * i;
            if let Some(entry) = quarantine.record_failure(now).quarantined {
                armed = Some((now, entry));
                break;
            }
        }
        let (armed_at, entry) = armed.expect("a persistent failing run must park the card");
        assert_eq!(entry.retry_in, GPU_QUARANTINE_BASE_BACKOFF);
        assert!(armed_at.saturating_duration_since(start) >= GPU_QUARANTINE_MIN_ELAPSED);
        assert!(matches!(
            quarantine.gate(armed_at),
            GpuGate::Skip { level: 1, .. }
        ));
        // And it always comes back: the card is re-probed, never latched off.
        assert!(matches!(
            quarantine.gate(armed_at + GPU_QUARANTINE_BASE_BACKOFF + Duration::from_secs(1)),
            GpuGate::Reprobe { .. }
        ));
        assert!(quarantine.record_success());
    }

    #[test]
    fn nonce_verification_rejects_a_short_intro_instead_of_blaming_the_gpu() {
        // An 83..88 byte intro used to pass the guard, get bytes 79..83 rewritten
        // and then be hashed over a different message length than the kernel used.
        // The guaranteed mismatch was reported as a GPU integrity error and
        // charged against the card's failure budget for a host-side bug.
        let height = 1u64;
        let short_intro = vec![0u8; 85];
        let err = verify_gpu_nonce_result(height, &short_intro, 0, 256, &(7, [0u8; 32]))
            .expect_err("an 85-byte intro must be rejected as a host-side length bug");
        assert!(
            err.contains("89 bytes"),
            "the error must name the 89-byte invariant, got: {err}"
        );
        assert!(
            verify_gpu_nonce_result(
                height,
                &vec![0u8; BLOCK_INTRO_BYTES + 1],
                0,
                256,
                &(7, [0u8; 32])
            )
            .is_err()
        );
    }

    #[test]
    fn gpu_error_recovery_is_bounded_and_accounts_only_mined_nonces() {
        use std::cell::Cell;

        let observed_space = Cell::new(0u32);
        let result = cpu_gpu_error_recovery(
            1,
            vec![0u8; 89],
            7,
            GPU_ERROR_CPU_RECOVERY_NONCES.saturating_mul(8),
            |_, _, nonce_start, nonce_space| {
                observed_space.set(nonce_space);
                (nonce_start, [0u8; 32])
            },
        );

        assert_eq!(observed_space.get(), GPU_ERROR_CPU_RECOVERY_NONCES);
        assert_eq!(result.gpu_nonce_space, 0);
        assert_eq!(result.cpu_nonce_space, GPU_ERROR_CPU_RECOVERY_NONCES);
        assert_eq!(result.head_nonce, 7);
    }

    /// Hash of `nonce` under a fixed 89-byte intro, i.e. what an honest card
    /// returns and what the CPU check recomputes.
    fn honest_hit(height: u64, block_intro: &[u8], nonce: u32) -> (u32, [u8; 32]) {
        let mut intro = block_intro.to_vec();
        intro[79..83].copy_from_slice(&nonce.to_be_bytes());
        (nonce, x16rs::block_hash(height, &intro))
    }

    #[test]
    fn a_full_share_buffer_reports_what_it_could_not_store() {
        // The capacity is fixed, so what matters is that the kernel's counter is
        // the TOTAL and the overflow is surfaced. Losing shares silently is
        // losing money silently, which is the whole defect being fixed.
        assert_eq!(split_share_readback(0, 1024), (0, 0));
        assert_eq!(split_share_readback(7, 1024), (7, 0));
        assert_eq!(split_share_readback(1024, 1024), (1024, 0));
        assert_eq!(split_share_readback(1025, 1024), (1024, 1));
        assert_eq!(split_share_readback(9_000, 1024), (1024, 7_976));
        // A degenerate capacity must not make the overflow look like zero.
        assert_eq!(split_share_readback(5, 0), (0, 5));

        // And the overflow travels with the batch instead of being dropped on
        // the floor between the kernel and the host.
        let batch = finish_gpu_batch(
            1,
            vec![0u8; BLOCK_INTRO_BYTES],
            0,
            256,
            GpuBatchOutcome {
                best: (3, [1u8; 32]),
                gpu_nonce_space: 256,
                shares: Vec::new(),
                share_overflow: 7_976,
            },
            |_, _, nonce_start, _| (nonce_start, [0u8; 32]),
        );
        assert_eq!(batch.share_overflow, 7_976);
    }

    #[test]
    fn a_batch_of_hits_yields_one_submission_each_not_one_for_the_batch() {
        // The measured defect: 34 billion hashes produced 77 submissions because
        // the card reported one result per batch. Every hit under the share
        // target has to become its own submission or pool credit tracks batch
        // cadence instead of hashrate.
        let height = 7u64;
        let block_intro = vec![0u8; BLOCK_INTRO_BYTES];
        let easiest_target = [0xffu8; 32];
        let raw: Vec<(u32, [u8; 32])> = (0..64u32)
            .map(|nonce| honest_hit(height, &block_intro, nonce))
            .collect();

        let shares =
            verify_gpu_shares(height, &block_intro, 0, 256, &easiest_target, &raw).unwrap();
        assert_eq!(shares.len(), 64);

        // The head result is whichever nonce the reduction picked; it is
        // submitted on its own, so the list must not repeat it.
        let best = raw[9];
        let batch = finish_gpu_batch(
            height,
            block_intro.clone(),
            0,
            256,
            GpuBatchOutcome {
                best,
                gpu_nonce_space: 256,
                shares,
                share_overflow: 0,
            },
            |_, _, nonce_start, _| (nonce_start, [0xffu8; 32]),
        );
        let submissions = batch.submission_nonces();
        assert_eq!(
            submissions.len(),
            64,
            "64 payable nonces must produce 64 submissions, not one"
        );
        assert_eq!(submissions[0], best.0);
        let mut sorted = submissions.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 64, "no nonce may be submitted twice");
    }

    #[test]
    fn a_share_the_card_cannot_prove_fails_the_batch_instead_of_reaching_the_pool() {
        let height = 7u64;
        let block_intro = vec![0u8; BLOCK_INTRO_BYTES];
        let easiest_target = [0xffu8; 32];
        let good = honest_hit(height, &block_intro, 11);

        // A hash the CPU does not reproduce.
        let mut wrong_hash = good;
        wrong_hash.0 = 12;
        assert!(
            verify_gpu_shares(height, &block_intro, 0, 256, &easiest_target, &[good, wrong_hash])
                .is_err()
        );
        // A nonce outside the batch window.
        assert!(
            verify_gpu_shares(
                height,
                &block_intro,
                0,
                8,
                &easiest_target,
                &[honest_hit(height, &block_intro, 900)]
            )
            .is_err()
        );
        // An honest hash the card listed even though it is ABOVE the target it
        // was told to filter on: the compare is broken, so nothing is forwarded.
        let strict_target = [0u8; 32];
        assert!(
            verify_gpu_shares(height, &block_intro, 0, 256, &strict_target, &[good]).is_err()
        );
    }

    #[test]
    fn solo_mining_returns_the_same_single_result_it_always_did() {
        // (d) of the brief: with no pool there is no share target, the OpenCL
        // kernel is launched with share_capacity=0 and skips the list entirely,
        // so a batch carries exactly one result, the same nonce and the same
        // hash as before the list existed.
        let height = 7u64;
        let block_intro = vec![0u8; BLOCK_INTRO_BYTES];
        let best = honest_hit(height, &block_intro, 42);

        let solo_ctx = BatchCtx {
            height,
            block_intro: block_intro.clone(),
            nonce_start: 0,
            nonce_space: 256,
            configured_wg: 1,
            localsize: 256,
            unitsize: 1,
            thermal_wg_cap: None,
            share_target: None,
        };
        assert!(
            solo_ctx.share_target.is_none(),
            "a solo template must never carry a share target"
        );

        let batch = finish_gpu_batch(
            height,
            block_intro,
            solo_ctx.nonce_start,
            solo_ctx.nonce_space,
            GpuBatchOutcome::best_only(best, 256),
            |_, _, nonce_start, _| (nonce_start, [0xffu8; 32]),
        );
        assert_eq!(batch.head_nonce, best.0);
        assert_eq!(batch.result_hash, best.1);
        assert!(batch.shares.is_empty());
        assert_eq!(batch.share_overflow, 0);
        assert_eq!(batch.submission_nonces(), vec![best.0]);
        assert_eq!(batch.gpu_nonce_space, 256);
        assert_eq!(batch.cpu_nonce_space, 0);
    }

    #[test]
    fn a_cpu_batch_never_carries_shares() {
        let batch = cpu_batch_fallback(1, vec![0u8; BLOCK_INTRO_BYTES], 5, 16, |_, _, ns, _| {
            (ns, [0u8; 32])
        });
        assert!(batch.shares.is_empty());
        assert_eq!(batch.share_overflow, 0);
        assert_eq!(batch.submission_nonces(), vec![5]);
    }
}
