//! Per-GPU handle: OOM recovery, context rebuild, snapshots.

use std::sync::Mutex;
use std::sync::atomic::AtomicU32;

use crate::gpu_arch::ArchLimits;
use crate::gpu_oom::{
    GpuBatchError, GpuGate, GpuOomState, GpuQuarantine, GpuQuarantineStatus, format_backoff,
};
use crate::mining_runtime::MiningRuntimeState;
use crate::opencl_diag::OpenClScan;

use super::init::initialize_opencl;
use super::resources::{OpenCLResources, soft_recover_opencl};

#[derive(Clone)]
pub struct OpenclGpuSnapshot {
    pub diamond_mining: bool,
    pub opencldir: String,
    pub platform_id: u32,
    pub device_id: u32,
    pub localsize: u32,
    pub unitsize: u32,
    pub amd_icd_count: usize,
}

pub fn opencl_snapshot_from_resource(
    res: &OpenCLResources,
    diamond_mining: bool,
    opencldir: &str,
    localsize: u32,
    unitsize: u32,
    amd_icd_count: usize,
) -> OpenclGpuSnapshot {
    OpenclGpuSnapshot {
        diamond_mining,
        opencldir: opencldir.to_string(),
        platform_id: res.platform_index,
        device_id: res.device_index,
        localsize,
        unitsize,
        amd_icd_count,
    }
}

pub struct OpenclGpuHandle {
    inner: Mutex<OpenCLResources>,
    snapshot: OpenclGpuSnapshot,
    oom: Mutex<GpuOomState>,
    /// Failures since the last clean batch OR the last successful context rebuild.
    /// Drives the rebuild cadence only. The quarantine keeps its own count, which a
    /// rebuild must NOT reset, otherwise a card that rebuilds every few errors is
    /// retried forever with no alert.
    consecutive_errors: AtomicU32,
    /// Time-based quarantine with exponential backoff and automatic re-probe. It
    /// replaces the old write-once session latch, which had no clearing path: a
    /// card that failed 20 batches during a driver reset stayed off until someone
    /// restarted the process. Identical policy to the CUDA backend.
    quarantine: GpuQuarantine,
    cached_scan: Mutex<Option<OpenClScan>>,
}

impl OpenclGpuHandle {
    pub fn new(
        res: OpenCLResources,
        snapshot: OpenclGpuSnapshot,
        scan: OpenClScan,
    ) -> std::sync::Arc<Self> {
        let base_wg = res.workgroups;
        std::sync::Arc::new(Self {
            inner: Mutex::new(res),
            snapshot,
            oom: Mutex::new(GpuOomState::new(base_wg)),
            consecutive_errors: AtomicU32::new(0),
            quarantine: GpuQuarantine::new(),
            cached_scan: Mutex::new(Some(scan)),
        })
    }

    /// Gate one batch. True while this device is quarantined and must not be given
    /// work; the caller mines the bounded CPU recovery window instead. When the
    /// backoff expires this rebuilds the context at floor work_groups and lets one
    /// re-probe batch through, so the card recovers with nobody watching.
    pub fn quarantine_blocks_batch(&self) -> bool {
        match self.quarantine.gate(std::time::Instant::now()) {
            GpuGate::Run => false,
            GpuGate::Skip {
                level,
                retry_in,
                total_failures,
                notify,
            } => {
                if notify {
                    eprintln!(
                        "[OpenCL] GPU QUARANTINED (level {level}, {total_failures} failed batches): no GPU work for another {}, mining continues on capped CPU recovery. The card is re-probed automatically, no restart needed.",
                        format_backoff(retry_in)
                    );
                }
                true
            }
            GpuGate::Reprobe {
                level,
                total_failures,
            } => {
                let wg = self.prepare_reprobe();
                println!(
                    "[OpenCL] GPU quarantine (level {level}, {total_failures} failed batches) expired: re-probing the device at work_groups={wg}."
                );
                false
            }
        }
    }

    /// Compatibility name for [`Self::quarantine_blocks_batch`], kept for the
    /// diamond worker's call site. There is no longer a permanent "disabled"
    /// state: this is true only while the device is inside a backoff window.
    pub fn gpu_is_disabled(&self) -> bool {
        self.quarantine_blocks_batch()
    }

    /// Quarantine state for the panel / stats (None while mining normally).
    pub fn quarantine_status(&self) -> Option<GpuQuarantineStatus> {
        self.quarantine.status(std::time::Instant::now())
    }

    /// One-line GPU health for a non-technical operator, e.g.
    /// `quarantined (level 3), retry in 8m, 61 failed batches`.
    pub fn quarantine_note(&self) -> String {
        self.quarantine.describe(std::time::Instant::now())
    }

    /// Drop to floor work_groups and rebuild the context before a re-probe, so the
    /// probe runs on the smallest, most likely to succeed configuration after the
    /// driver has had the whole backoff window to recover.
    fn prepare_reprobe(&self) -> u32 {
        let mut res = self.lock_resources();
        let floor = {
            let mut oom = self.oom.lock().unwrap_or_else(|e| e.into_inner());
            let floor = oom.floor_wg();
            oom.sync_effective(floor);
            floor
        };
        soft_recover_opencl(&mut res);
        let scan = self
            .cached_scan
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(scan) = scan {
            match rebuild_opencl_gpu(&self.snapshot, floor, &scan) {
                Ok(new_res) => {
                    let synced_wg = new_res.workgroups;
                    *res = new_res;
                    drop(res);
                    if let Ok(mut oom) = self.oom.lock() {
                        oom.sync_effective(synced_wg);
                    }
                    return synced_wg;
                }
                Err(e) => eprintln!("[OpenCL] re-probe context rebuild failed: {}", e),
            }
        }
        floor
    }

    pub fn configure_oom_floor(
        &self,
        vram_bytes: u64,
        localsize: u32,
        unitsize: u32,
        configured: u32,
        arch_slug: &str,
    ) {
        if let Ok(mut oom) = self.oom.lock() {
            oom.configure_floor(vram_bytes, localsize, unitsize, configured, arch_slug);
        }
    }

    pub fn lock_resources(&self) -> std::sync::MutexGuard<'_, OpenCLResources> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn sensor_identity(&self) -> (crate::gpu_arch::GpuVendor, u32) {
        let resources = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        (resources.vendor, resources.device_index)
    }

    pub fn workgroups(&self, configured: u32, thermal_cap: Option<u32>) -> u32 {
        let res_wg = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .workgroups;
        self.oom
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .workgroups(res_wg.min(configured), thermal_cap)
    }

    pub fn effective_wg(&self) -> u32 {
        self.oom
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .effective_wg()
    }

    pub fn on_batch_error(
        &self,
        err: GpuBatchError,
        oom_fallback: bool,
        configured_wg: u32,
        runtime: &MiningRuntimeState,
    ) {
        runtime.record_gpu_error_event();
        use std::sync::atomic::Ordering::Relaxed;
        let report = self.quarantine.record_failure(std::time::Instant::now());
        let mut res = self.lock_resources();
        let res_wg = res.workgroups;
        let arch_limits = ArchLimits::for_slug(&res.arch_slug);
        let experimental = arch_limits.is_experimental();
        let mut oom = self.oom.lock().unwrap_or_else(|e| e.into_inner());
        let cur_eff = oom.effective_wg();
        let at_floor = cur_eff <= oom.floor_wg();
        let n = self.consecutive_errors.fetch_add(1, Relaxed) + 1;
        // Same policy as CUDA, and deliberately NOT conditioned on being at the
        // work_groups floor: an arch whose floor is never reached used to be
        // retried forever with no alert at all. Park the device for a growing
        // interval instead of latching it off for the process.
        if let Some(entry) = report.quarantined {
            let floor = oom.floor_wg();
            oom.sync_effective(floor);
            drop(oom);
            soft_recover_opencl(&mut res);
            drop(res);
            runtime.report_gpu_workgroups(floor, runtime.thermal_workgroups_cap(), configured_wg);
            eprintln!(
                "[OpenCL] ALERT GPU quarantined after {} consecutive failed batches ({} this session): no GPU work for {}, then the card is re-probed automatically. Mining continues on capped CPU recovery. Last error: {}. Check the driver, cooling, power and the PCIe riser.",
                report.consecutive_failures,
                report.total_failures,
                format_backoff(entry.retry_in),
                err.display()
            );
            return;
        }
        let retry_only =
            experimental && err.is_out_of_resources() && oom_fallback && !at_floor && n < 3;
        let next_wg = if retry_only {
            cur_eff
        } else {
            oom.record_error(res_wg, oom_fallback)
        };
        let wg_reduced = next_wg < cur_eff;
        drop(oom);
        let thermal = runtime.thermal_workgroups_cap();
        runtime.report_gpu_workgroups(next_wg, thermal, configured_wg);
        soft_recover_opencl(&mut res);
        let should_rebuild = if at_floor && experimental && err.is_out_of_resources() && n >= 5 {
            true
        } else {
            wg_reduced && (n >= 2 || err.is_out_of_resources())
        };
        if should_rebuild {
            let rebuild_wg = if wg_reduced { next_wg } else { cur_eff.max(1) };
            let scan = self
                .cached_scan
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(scan) = scan {
                match rebuild_opencl_gpu(&self.snapshot, rebuild_wg, &scan) {
                    Ok(new_res) => {
                        let synced_wg = new_res.workgroups;
                        *res = new_res;
                        if let Ok(mut oom) = self.oom.lock() {
                            oom.sync_effective(synced_wg);
                        }
                        self.consecutive_errors.store(0, Relaxed);
                        eprintln!(
                            "[OpenCL] Rebuilt GPU context (errors={}, work_groups={})",
                            n, rebuild_wg
                        );
                    }
                    Err(e) => eprintln!("[OpenCL] Context rebuild failed: {}", e),
                }
            }
            drop(res);
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    pub fn on_batch_success(&self, configured_wg: u32, runtime: &MiningRuntimeState) {
        use std::sync::atomic::Ordering::Relaxed;
        self.consecutive_errors.store(0, Relaxed);
        if self.quarantine.record_success() {
            println!(
                "[OpenCL] GPU RECOVERED: the re-probe succeeded, quarantine cleared and GPU mining has resumed."
            );
        }
        self.oom
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record_success();
        self.grow_context_to_effective(configured_wg);
        runtime.report_gpu_workgroups(
            self.effective_wg(),
            runtime.thermal_workgroups_cap(),
            configured_wg,
        );
    }

    /// After an OOM ramp-back the OOM state can allow more work_groups than the
    /// current context has buffers for, and `enqueue_mining_kernel` rejects
    /// anything above `res.workgroups`. Rebuild once at the larger size, otherwise
    /// the ramp is purely cosmetic and the card stays at reduced throughput for
    /// the rest of the session.
    fn grow_context_to_effective(&self, configured_wg: u32) {
        let want = self.effective_wg().min(configured_wg.max(1));
        let mut res = self.lock_resources();
        if want <= res.workgroups {
            return;
        }
        let scan = self
            .cached_scan
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let rebuilt = match scan {
            Some(scan) => rebuild_opencl_gpu(&self.snapshot, want, &scan),
            None => Err("no cached OpenCL scan".to_string()),
        };
        match rebuilt {
            Ok(new_res) => {
                let synced_wg = new_res.workgroups;
                *res = new_res;
                drop(res);
                if let Ok(mut oom) = self.oom.lock() {
                    oom.sync_effective(synced_wg);
                }
                println!("[OpenCL] Restored GPU context at work_groups={}", synced_wg);
            }
            Err(e) => {
                // Clamp the OOM state back to what the live context can run, so a
                // failed ramp-up is not retried on every following batch.
                let capped = res.workgroups;
                drop(res);
                if let Ok(mut oom) = self.oom.lock() {
                    oom.sync_effective(capped);
                }
                eprintln!(
                    "[OpenCL] work_groups ramp-up rebuild failed, staying at {}: {}",
                    capped, e
                );
            }
        }
    }
}

fn rebuild_opencl_gpu(
    snapshot: &OpenclGpuSnapshot,
    workgroups: u32,
    scan: &OpenClScan,
) -> std::result::Result<OpenCLResources, String> {
    let device_ids = snapshot.device_id.to_string();
    let mut devices = initialize_opencl(
        snapshot.diamond_mining,
        &snapshot.opencldir,
        &snapshot.platform_id,
        &device_ids,
        &workgroups,
        &snapshot.localsize,
        &snapshot.unitsize,
        Some(scan),
        true,
    );
    if devices.is_empty() {
        return Err("OpenCL reinit returned no devices".into());
    }
    Ok(devices.remove(0))
}
