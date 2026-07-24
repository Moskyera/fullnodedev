use x16rs_cuda::{CudaMiner, CudaResult};

pub struct CudaMiningResources {
    pub miner: CudaMiner,
    pub workgroups: u32,
    pub unit_size: u32,
    /// Effective work-groups after OOM/error backoff. Starts at `workgroups` and
    /// is halved on a batch error, ramped back up on success, mirroring the
    /// OpenCL GpuOomState. Be honest about what this buys: a smaller grid only
    /// mitigates launch-timeout / TDR pressure. A block-size resource fault
    /// (cudaErrorInvalidConfiguration / LaunchOutOfResources) and a real
    /// out-of-memory failure are NOT fixed by fewer work groups, and a sticky
    /// context fault is grid independent, so those keep failing until the
    /// consecutive-failure alert disables the card (see mining_batch.rs).
    pub eff_wg: std::sync::atomic::AtomicU32,
    /// Never back off below this many work-groups.
    pub floor_wg: u32,
    /// Failed batches since the last clean one. Bounds how long a dead card may
    /// keep being retried before the operator is told and the GPU is dropped.
    pub consecutive_errors: std::sync::atomic::AtomicU32,
    /// Set once this card has been given up on for the rest of the process.
    pub gpu_disabled: std::sync::atomic::AtomicBool,
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
        self.consecutive_errors
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let cur = self.eff_wg.load(std::sync::atomic::Ordering::Relaxed);
        if cur < self.workgroups {
            let next = cur.saturating_add((cur / 4).max(1)).min(self.workgroups);
            self.eff_wg
                .store(next, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Count one failed batch and return the new consecutive-failure total.
    pub fn note_batch_failure(&self) -> u32 {
        self.consecutive_errors
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(1)
    }

    /// Stop using this card for the rest of the process. Returns true only for the
    /// caller that flipped it, so the loud operator alert is printed exactly once.
    pub fn disable_gpu_for_session(&self) -> bool {
        !self
            .gpu_disabled
            .swap(true, std::sync::atomic::Ordering::Relaxed)
    }

    /// True once the card has been given up on for this session.
    pub fn gpu_is_disabled(&self) -> bool {
        self.gpu_disabled
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub fn initialize_cuda(
    device_index: i32,
    workgroups: u32,
    unit_size: u32,
) -> Vec<Arc<CudaMiningResources>> {
    if !CudaMiner::is_available() {
        eprintln!(
            "[CUDA] x16rs-cuda built without kernels; rebuild with: cargo build -p poworker --features cuda"
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
                consecutive_errors: std::sync::atomic::AtomicU32::new(0),
                gpu_disabled: std::sync::atomic::AtomicBool::new(false),
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