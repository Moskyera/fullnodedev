//! Per-GPU OpenCL work_groups OOM recovery (halving + optional ramp-back).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

use crate::efficiency::clamp_workgroups_for_vram;
use crate::gpu_arch::ArchLimits;

/// Minimum work_groups after OOM fallback on standard AMD/NVIDIA paths.
pub const OOM_FLOOR_WG: u32 = 512;
/// Successful GPU batches before restoring base work_groups after OOM reduction.
pub const OOM_RECOVERY_BATCHES: u32 = 16;

/// Per-device work_groups state — lives on [`crate::opencl_gpu::OpenclGpuHandle`].
pub struct GpuOomState {
    base_workgroups: u32,
    effective_workgroups: AtomicU32,
    oom_floor_wg: u32,
    oom_reduced: AtomicBool,
    success_batches_since_oom: AtomicU32,
    oom_ramp_to_base: bool,
}

impl GpuOomState {
    pub fn new(base_workgroups: u32) -> Self {
        let wg = base_workgroups.max(1);
        Self {
            base_workgroups: wg,
            effective_workgroups: AtomicU32::new(wg),
            oom_floor_wg: OOM_FLOOR_WG,
            oom_reduced: AtomicBool::new(false),
            success_batches_since_oom: AtomicU32::new(0),
            oom_ramp_to_base: true,
        }
    }

    pub fn configure_floor(
        &mut self,
        vram_bytes: u64,
        localsize: u32,
        unitsize: u32,
        configured: u32,
        arch_slug: &str,
    ) {
        let limits = ArchLimits::for_slug(arch_slug);
        if !limits.oom_ramp_to_base {
            self.oom_floor_wg = limits.oom_floor_wg.min(configured.max(1));
            self.oom_ramp_to_base = false;
            return;
        }
        self.oom_ramp_to_base = true;
        let floor = if vram_bytes > 0 {
            clamp_workgroups_for_vram(vram_bytes, localsize, unitsize, configured)
        } else {
            OOM_FLOOR_WG
        };
        self.oom_floor_wg = floor.max(OOM_FLOOR_WG);
    }

    pub fn effective_wg(&self) -> u32 {
        self.effective_workgroups.load(Relaxed)
    }

    pub fn workgroups(&self, configured: u32, thermal_cap: Option<u32>) -> u32 {
        let mut wg = self.effective_workgroups.load(Relaxed).max(1);
        wg = wg.min(configured.max(1));
        if let Some(cap) = thermal_cap {
            wg = wg.min(cap.max(1));
        }
        wg
    }

    pub fn record_error(&self, configured: u32, oom_fallback: bool) -> u32 {
        if !oom_fallback {
            return self.workgroups(configured, None);
        }
        let cur = self.effective_workgroups.load(Relaxed).max(1);
        let floor = self.oom_floor_wg.max(1);
        let next = (cur / 2).max(floor);
        if next < cur {
            eprintln!(
                "[efficiency] OpenCL error — reducing work_groups {} -> {} (floor={})",
                cur, next, floor
            );
            self.effective_workgroups.store(next, Relaxed);
            self.oom_reduced.store(true, Relaxed);
            self.success_batches_since_oom.store(0, Relaxed);
        }
        next
    }

    pub fn floor_wg(&self) -> u32 {
        self.oom_floor_wg.max(1)
    }

    pub fn sync_effective(&mut self, wg: u32) {
        let clamped = wg.max(1);
        self.effective_workgroups.store(clamped, Relaxed);
    }

    pub fn record_success(&self) {
        if !self.oom_ramp_to_base {
            return;
        }
        let cur = self.effective_workgroups.load(Relaxed);
        let base = self.base_workgroups.max(1);
        if cur >= base {
            self.oom_reduced.store(false, Relaxed);
            self.success_batches_since_oom.store(0, Relaxed);
            return;
        }
        if !self.oom_reduced.load(Relaxed) {
            return;
        }
        let n = self.success_batches_since_oom.fetch_add(1, Relaxed) + 1;
        if n >= OOM_RECOVERY_BATCHES {
            self.effective_workgroups.store(base, Relaxed);
            self.oom_reduced.store(false, Relaxed);
            self.success_batches_since_oom.store(0, Relaxed);
            println!(
                "[efficiency] GPU stable — restored work_groups to {}",
                base
            );
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GpuBatchError {
    OutOfResources,
    Other(String),
}

impl GpuBatchError {
    pub fn from_message(msg: &str) -> Self {
        if is_opencl_oom_error(msg) {
            Self::OutOfResources
        } else {
            Self::Other(msg.to_string())
        }
    }

    pub fn is_out_of_resources(&self) -> bool {
        matches!(self, Self::OutOfResources)
    }

    pub fn display(&self) -> String {
        match self {
            Self::OutOfResources => "CL_OUT_OF_RESOURCES".to_string(),
            Self::Other(s) => s.clone(),
        }
    }
}

pub fn is_opencl_oom_error(err: &str) -> bool {
    err.contains("OUT_OF_RESOURCES")
        || err.contains("Out of resources")
        || err.contains("out of resources")
        || err.contains("CL_OUT_OF_RESOURCES")
        || err.contains("error code -5")
        || err.contains("error -5")
}

#[cfg(feature = "ocl")]
pub fn from_ocl_error(err: &ocl::Error) -> GpuBatchError {
    let msg = err.to_string();
    GpuBatchError::from_message(&msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oom_fallback_stops_at_512_floor() {
        let st = GpuOomState::new(2048);
        st.record_error(2048, true);
        assert_eq!(st.effective_workgroups.load(Relaxed), 1024);
        st.record_error(2048, true);
        assert_eq!(st.effective_workgroups.load(Relaxed), 512);
        st.record_error(2048, true);
        assert_eq!(st.effective_workgroups.load(Relaxed), 512);
    }

    #[test]
    fn oom_recovery_restores_base_workgroups() {
        let st = GpuOomState::new(1024);
        st.record_error(1024, true);
        assert_eq!(st.effective_workgroups.load(Relaxed), 512);
        for _ in 0..OOM_RECOVERY_BATCHES {
            st.record_success();
        }
        assert_eq!(st.effective_workgroups.load(Relaxed), 1024);
    }

    #[test]
    fn gfx1201_configure_floor_uses_32_not_512() {
        let mut st = GpuOomState::new(512);
        st.configure_floor(16 * 1024 * 1024 * 1024, 256, 64, 512, "gfx1201");
        while st.effective_workgroups.load(Relaxed) > 32 {
            st.record_error(512, true);
        }
        assert_eq!(
            st.effective_workgroups.load(Relaxed),
            32,
            "gfx1201 halving must stop at 32, not generic 512 floor"
        );
        st.record_error(512, true);
        assert_eq!(st.effective_workgroups.load(Relaxed), 32);
    }

    #[test]
    fn gfx1201_oom_stays_at_floor_without_ramp() {
        let mut st = GpuOomState::new(64);
        st.configure_floor(16 * 1024 * 1024 * 1024, 256, 64, 64, "gfx1201");
        st.record_error(64, true);
        assert_eq!(st.effective_workgroups.load(Relaxed), 32);
        for _ in 0..OOM_RECOVERY_BATCHES * 2 {
            st.record_success();
        }
        assert_eq!(
            st.effective_workgroups.load(Relaxed),
            32,
            "gfx1201 must not ramp back to OOM-prone base"
        );
    }
}