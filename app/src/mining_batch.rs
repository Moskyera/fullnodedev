//! Shared CPU/GPU batch merge helpers and block-miner backend trait.

use std::sync::Arc;

use crate::hash_util::hash_more_power;

#[cfg(feature = "ocl")]
use crate::mining_runtime::MiningRuntimeState;
#[cfg(feature = "ocl")]
use crate::opencl_gpu::block::do_group_block_mining_opencl;
#[cfg(feature = "ocl")]
use crate::opencl_gpu::OpenclGpuHandle;

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
        let wg_cap = self
            .gpu
            .workgroups(ctx.configured_wg, ctx.thermal_wg_cap);
        let Some(plan) = plan_gpu_batch(
            ctx.nonce_space,
            wg_cap,
            ctx.localsize,
            ctx.unitsize,
        ) else {
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
                self.gpu.on_batch_error(
                    e,
                    self.oom_fallback,
                    ctx.configured_wg,
                    &self.runtime,
                );
                cpu_batch_fallback(
                    ctx.height,
                    ctx.block_intro.clone(),
                    ctx.nonce_start,
                    ctx.nonce_space,
                    cpu_mine,
                )
            }
            Ok(best) => {
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
    pub cuda: Arc<crate::CudaMiningResources>,
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

        match crate::do_group_block_mining_cuda(
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