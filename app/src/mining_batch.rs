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
use crate::opencl_gpu::block::do_group_block_mining_opencl;

#[cfg(any(feature = "ocl", test))]
const GPU_ERROR_CPU_RECOVERY_NONCES: u32 = 100_000;

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
}

/// Result of one batch including GPU/CPU nonce accounting for stats.
pub struct BatchResult {
    pub head_nonce: u32,
    pub result_hash: [u8; 32],
    pub gpu_nonce_space: u32,
    pub cpu_nonce_space: u32,
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

/// Compare two 32-byte hashes; returns true if `candidate` beats `current`.
pub fn hash_beats(candidate: &[u8; 32], current: &[u8; 32]) -> bool {
    hash_more_power(candidate, current)
}
/// Verify the GPU's best nonce/hash pair before it can reach submission.
pub fn verify_gpu_best_result(
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
    if block_intro.len() < 83 {
        return Err("block intro is too short for nonce verification".to_string());
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

/// Finish a GPU batch: merge CPU tail nonces into the best hash.
pub fn finish_gpu_batch(
    height: u64,
    block_intro: Vec<u8>,
    nonce_start: u32,
    nonce_space: u32,
    gpu_best: (u32, [u8; 32]),
    gpu_nonce_space: u32,
    cpu_mine: impl Fn(u64, Vec<u8>, u32, u32) -> (u32, [u8; 32]),
) -> BatchResult {
    let tail_space = nonce_space.saturating_sub(gpu_nonce_space);
    let (head_nonce, result_hash) = if tail_space > 0 {
        let tail_start = nonce_start.saturating_add(gpu_nonce_space);
        merge_cpu_tail(
            gpu_best,
            height,
            block_intro,
            tail_start,
            tail_space,
            cpu_mine,
        )
    } else {
        gpu_best
    };
    BatchResult {
        head_nonce,
        result_hash,
        gpu_nonce_space,
        cpu_nonce_space: tail_space,
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
    }
}

/// Recover a bounded prefix after an OpenCL failure and skip the rest of the failed window.
#[cfg(any(feature = "ocl", test))]
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
            do_group_block_mining_opencl(
                &opencl,
                ctx.height,
                ctx.block_intro.clone(),
                ctx.nonce_start,
                plan.workgroups_eff,
                ctx.localsize,
                ctx.unitsize,
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
            Ok(best) => {
                if let Err(message) = verify_gpu_best_result(
                    ctx.height,
                    &ctx.block_intro,
                    ctx.nonce_start,
                    plan.gpu_nonce_space,
                    &best,
                ) {
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
                self.gpu.on_batch_success(ctx.configured_wg, &self.runtime);
                finish_gpu_batch(
                    ctx.height,
                    ctx.block_intro.clone(),
                    ctx.nonce_start,
                    ctx.nonce_space,
                    best,
                    plan.gpu_nonce_space,
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
        let wg_cap = self.cuda.workgroups.min(self.configured_wg);
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
                cpu_batch_fallback(
                    ctx.height,
                    ctx.block_intro.clone(),
                    ctx.nonce_start,
                    ctx.nonce_space,
                    cpu_mine,
                )
            }
            Ok(best) => finish_gpu_batch(
                ctx.height,
                ctx.block_intro.clone(),
                ctx.nonce_start,
                ctx.nonce_space,
                best,
                plan.gpu_nonce_space,
                cpu_mine,
            ),
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

        verify_gpu_best_result(height, &block_intro, nonce_start, 256, &valid).unwrap();

        let mut bad_hash = result_hash;
        bad_hash[0] ^= 1;
        assert!(
            verify_gpu_best_result(
                height,
                &block_intro,
                nonce_start,
                256,
                &(result_nonce, bad_hash)
            )
            .is_err()
        );
        assert!(
            verify_gpu_best_result(
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
}
