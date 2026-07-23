use x16rs_cuda::{CudaMiner, CudaResult};

pub struct CudaMiningResources {
    pub miner: CudaMiner,
    pub workgroups: u32,
    pub unit_size: u32,
    /// Effective work-groups after OOM/error backoff. Starts at `workgroups` and
    /// is halved on a batch error, ramped back up on success — mirroring the
    /// OpenCL GpuOomState so a CUDA card that fails at its configured size adapts
    /// down to one that runs instead of failing every batch forever.
    pub eff_wg: std::sync::atomic::AtomicU32,
    /// Never back off below this many work-groups.
    pub floor_wg: u32,
}

impl CudaMiningResources {
    /// Current effective work-groups, clamped to [floor, configured].
    pub fn effective_wg(&self) -> u32 {
        let max = self.workgroups.max(1);
        let floor = self.floor_wg.max(1).min(max);
        self.eff_wg
            .load(std::sync::atomic::Ordering::Relaxed)
            .clamp(floor, max)
    }

    /// Halve the effective work-groups toward the floor after a batch error/OOM.
    pub fn record_error(&self) -> u32 {
        let floor = self.floor_wg.max(1);
        let cur = self.eff_wg.load(std::sync::atomic::Ordering::Relaxed);
        let next = (cur / 2).max(floor);
        self.eff_wg
            .store(next, std::sync::atomic::Ordering::Relaxed);
        next
    }

    /// Ramp the effective work-groups back up toward the configured maximum after
    /// a clean batch, so throughput recovers once memory pressure clears.
    pub fn record_success(&self) {
        let cur = self.eff_wg.load(std::sync::atomic::Ordering::Relaxed);
        if cur < self.workgroups {
            let next = cur.saturating_add((cur / 4).max(1)).min(self.workgroups);
            self.eff_wg
                .store(next, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

pub fn initialize_cuda(
    device_index: i32,
    workgroups: u32,
    unit_size: u32,
) -> Vec<Arc<CudaMiningResources>> {
    if !CudaMiner::is_available() {
        eprintln!(
            "[CUDA] x16rs-cuda built without kernels — rebuild with: cargo build -p poworker --features cuda"
        );
        return Vec::new();
    }

    match CudaMiner::list_devices() {
        Ok(devices) => {
            for d in &devices {
                println!(
                    "[CUDA] Device #{}: {} (SM {}.{}, MP={})",
                    d.index, d.name, d.compute_major, d.compute_minor, d.multiprocessor_count
                );
            }
        }
        Err(e) => {
            eprintln!("[CUDA] enumerate devices failed: {e}");
            return Vec::new();
        }
    }

    match CudaMiner::new(device_index, workgroups, unit_size) {
        Ok(miner) => {
            println!(
                "[CUDA] Initialized device #{device_index} work_groups={workgroups} unit_size={unit_size}"
            );
            vec![Arc::new(CudaMiningResources {
                miner,
                workgroups,
                unit_size,
                eff_wg: std::sync::atomic::AtomicU32::new(workgroups),
                floor_wg: (workgroups / 16).max(1),
            })]
        }
        Err(e) => {
            eprintln!("[CUDA] init failed: {e}");
            Vec::new()
        }
    }
}

pub fn do_group_block_mining_cuda(
    cuda: &CudaMiningResources,
    height: u64,
    block_intro: Vec<u8>,
    nonce_start: u32,
    workgroups: u32,
) -> CudaResult<(u32, [u8; 32])> {
    cuda.miner
        .mine_block_batch(height, &block_intro, nonce_start, workgroups)
}