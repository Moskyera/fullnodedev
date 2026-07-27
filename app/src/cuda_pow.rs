use x16rs_cuda::{CudaBatchOutput, CudaMiner, CudaResult};

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
    /// quarantine parks the card for a backoff window (see `quarantine` below).
    pub eff_wg: std::sync::atomic::AtomicU32,
    /// Never back off below this many work-groups.
    pub floor_wg: u32,
    /// Time-based quarantine with exponential backoff and automatic re-probe,
    /// shared with the OpenCL backend so both cards are treated identically. It
    /// also owns the consecutive-failure count, which only a clean batch resets.
    pub quarantine: crate::gpu_oom::GpuQuarantine,
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
        if self.quarantine.record_success() {
            println!(
                "[CUDA] GPU RECOVERED: the re-probe succeeded, quarantine cleared and GPU mining has resumed."
            );
        }
        let cur = self.eff_wg.load(std::sync::atomic::Ordering::Relaxed);
        if cur < self.workgroups {
            let next = cur.saturating_add((cur / 4).max(1)).min(self.workgroups);
            self.eff_wg
                .store(next, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Count one failed batch and report whether it parked the card.
    pub fn note_batch_failure(&self) -> crate::gpu_oom::GpuFailureReport {
        self.quarantine.record_failure(std::time::Instant::now())
    }

    /// Print the one-time operator alert when a failure armed the quarantine.
    pub fn announce_quarantine(&self, report: &crate::gpu_oom::GpuFailureReport, detail: &str) {
        let Some(entry) = report.quarantined else {
            return;
        };
        // Drop straight to the floor so the automatic re-probe runs on the
        // smallest, most likely to succeed grid.
        self.eff_wg
            .store(self.floor_wg.max(1), std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "[CUDA] ALERT GPU quarantined after {} consecutive failed batches ({} this session): no GPU work for {}, then the card is re-probed automatically. Mining continues on capped CPU recovery. {} Check the driver, cooling, power and the PCIe riser.",
            report.consecutive_failures,
            report.total_failures,
            crate::gpu_oom::format_backoff(entry.retry_in),
            detail
        );
    }

    /// Gate one batch. True while the card is quarantined and must not be given
    /// work. When the backoff expires this lets exactly one re-probe batch through
    /// at floor work-groups, so the card recovers without a restart.
    pub fn quarantine_blocks_batch(&self) -> bool {
        match self.quarantine.gate(std::time::Instant::now()) {
            crate::gpu_oom::GpuGate::Run => false,
            crate::gpu_oom::GpuGate::Skip {
                level,
                retry_in,
                total_failures,
                notify,
            } => {
                if notify {
                    eprintln!(
                        "[CUDA] GPU QUARANTINED (level {level}, {total_failures} failed batches): no GPU work for another {}, mining continues on capped CPU recovery. The card is re-probed automatically, no restart needed.",
                        crate::gpu_oom::format_backoff(retry_in)
                    );
                }
                true
            }
            crate::gpu_oom::GpuGate::Reprobe {
                level,
                total_failures,
            } => {
                let wg = self.floor_wg.max(1);
                self.eff_wg.store(wg, std::sync::atomic::Ordering::Relaxed);
                println!(
                    "[CUDA] GPU quarantine (level {level}, {total_failures} failed batches) expired: re-probing the device at work_groups={wg}."
                );
                false
            }
        }
    }

    /// Quarantine state for the panel / stats (None while mining normally).
    pub fn quarantine_status(&self) -> Option<crate::gpu_oom::GpuQuarantineStatus> {
        self.quarantine.status(std::time::Instant::now())
    }

    /// One-line GPU health for a non-technical operator.
    pub fn quarantine_note(&self) -> String {
        self.quarantine.describe(std::time::Instant::now())
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
                quarantine: crate::gpu_oom::GpuQuarantine::new(),
            })]
        }
        Err(e) => {
            eprintln!("[CUDA] init failed: {e}");
            Vec::new()
        }
    }
}

/// One CUDA block batch, best result only.
///
/// Kept exactly as it was for the callers that want one hash per batch and nothing
/// else. Pool mining goes through [`do_group_block_mining_cuda_shares`].
pub fn do_group_block_mining_cuda(
    cuda: &CudaMiningResources,
    height: u64,
    block_intro: Vec<u8>,
    nonce_start: u32,
    workgroups: u32,
) -> CudaResult<(u32, [u8; 32])> {
    do_group_block_mining_cuda_shares(cuda, height, block_intro, nonce_start, workgroups, None)
        .map(|out| out.best)
}

/// One CUDA block batch, plus every nonce that beat `share_target`.
///
/// `share_target` is `None` for solo mining, and that is the whole guarantee that
/// solo behaviour is unchanged: the kernel is launched with share_capacity=0, skips
/// the appending block, and the host neither uploads a target nor reads a share
/// buffer back.
pub fn do_group_block_mining_cuda_shares(
    cuda: &CudaMiningResources,
    height: u64,
    block_intro: Vec<u8>,
    nonce_start: u32,
    workgroups: u32,
    share_target: Option<&[u8; 32]>,
) -> CudaResult<CudaBatchOutput> {
    cuda.miner.mine_block_batch_shares(
        height,
        &block_intro,
        nonce_start,
        workgroups,
        share_target,
    )
}
