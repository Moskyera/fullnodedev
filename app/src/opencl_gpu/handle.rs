//! Per-GPU handle: OOM recovery, context rebuild, snapshots.

use std::sync::atomic::AtomicU32;
use std::sync::Mutex;

use crate::gpu_arch::ArchLimits;
use crate::gpu_oom::{GpuBatchError, GpuOomState};
use crate::mining_runtime::MiningRuntimeState;
use crate::opencl_diag::OpenClScan;

use super::init::initialize_opencl;
use super::resources::{soft_recover_opencl, OpenCLResources};

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
    consecutive_errors: AtomicU32,
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
            cached_scan: Mutex::new(Some(scan)),
        })
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
        self.inner.lock().expect("opencl gpu mutex")
    }

    pub fn workgroups(
        &self,
        configured: u32,
        thermal_cap: Option<u32>,
    ) -> u32 {
        let res_wg = self.inner.lock().expect("opencl gpu mutex").workgroups;
        self.oom
            .lock()
            .expect("gpu oom mutex")
            .workgroups(res_wg.min(configured), thermal_cap)
    }

    pub fn effective_wg(&self) -> u32 {
        self.oom.lock().expect("gpu oom mutex").effective_wg()
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
        let mut res = self.lock_resources();
        let res_wg = res.workgroups;
        let arch_limits = ArchLimits::for_slug(&res.arch_slug);
        let experimental = arch_limits.is_experimental();
        let mut oom = self.oom.lock().expect("gpu oom mutex");
        let cur_eff = oom.effective_wg();
        let at_floor = cur_eff <= oom.floor_wg();
        let n = self.consecutive_errors.fetch_add(1, Relaxed) + 1;
        let retry_only = experimental
            && err.is_out_of_resources()
            && oom_fallback
            && !at_floor
            && n < 3;
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
            let scan = self.cached_scan.lock().expect("scan cache mutex").clone();
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
        self.oom.lock().expect("gpu oom mutex").record_success();
        runtime.report_gpu_workgroups(
            self.effective_wg(),
            runtime.thermal_workgroups_cap(),
            configured_wg,
        );
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
