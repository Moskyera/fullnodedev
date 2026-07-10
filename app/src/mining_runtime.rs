//! Process-wide mining runtime: CPU assist, thermal cap, profit pause — not per-GPU OOM.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;

use crate::efficiency::EfficiencyConf;

pub struct MiningRuntimeState {
    pub active_cpu_assist: AtomicU32,
    pub gpu_errors: AtomicU32,
    pub throttled: AtomicBool,
    pub paused_unprofitable: AtomicBool,
    pub adjust_counter: AtomicU64,
    /// When non-zero, caps per-GPU effective work_groups (thermal throttle).
    thermal_cap_wg: AtomicU32,
    /// Latest OOM-adjusted work_groups across GPU workers (0 = unknown).
    oom_work_groups_min: AtomicU32,
    /// Latest effective work_groups across GPU workers (OOM ∩ thermal ∩ configured).
    effective_work_groups_min: AtomicU32,
    /// Last writer for backward-compatible single-GPU panel reads.
    pub reported_effective_wg: AtomicU32,
}

impl MiningRuntimeState {
    pub fn new(_configured_workgroups: u32, active_cpu: u32) -> Arc<MiningRuntimeState> {
        Arc::new(MiningRuntimeState {
            active_cpu_assist: AtomicU32::new(active_cpu.max(1)),
            gpu_errors: AtomicU32::new(0),
            throttled: AtomicBool::new(false),
            paused_unprofitable: AtomicBool::new(false),
            adjust_counter: AtomicU64::new(0),
            thermal_cap_wg: AtomicU32::new(0),
            oom_work_groups_min: AtomicU32::new(0),
            effective_work_groups_min: AtomicU32::new(0),
            reported_effective_wg: AtomicU32::new(0),
        })
    }

    pub fn record_gpu_error_event(&self) {
        self.gpu_errors.fetch_add(1, Relaxed);
    }

    pub fn thermal_workgroups_cap(&self) -> Option<u32> {
        let cap = self.thermal_cap_wg.load(Relaxed);
        if cap == 0 {
            None
        } else {
            Some(cap)
        }
    }

    pub fn oom_work_groups(&self) -> u32 {
        self.oom_work_groups_min.load(Relaxed)
    }

    pub fn effective_work_groups(&self) -> u32 {
        self.effective_work_groups_min.load(Relaxed)
    }

    fn compute_effective(oom_wg: u32, thermal_cap: Option<u32>, configured_wg: u32) -> u32 {
        let mut effective = oom_wg.max(1);
        if let Some(cap) = thermal_cap {
            if cap > 0 {
                effective = effective.min(cap);
            }
        }
        if configured_wg > 0 {
            effective = effective.min(configured_wg);
        }
        effective
    }

    fn store_workgroup_stat(target: &AtomicU32, value: u32) {
        if value > 0 {
            target.store(value, Relaxed);
        }
    }

    /// Report per-GPU OOM work_groups; aggregates latest across devices for panel stats.
    pub fn report_gpu_workgroups(
        &self,
        oom_wg: u32,
        thermal_cap: Option<u32>,
        configured_wg: u32,
    ) {
        let effective = Self::compute_effective(oom_wg, thermal_cap, configured_wg);
        Self::store_workgroup_stat(&self.oom_work_groups_min, oom_wg);
        Self::store_workgroup_stat(&self.effective_work_groups_min, effective);
        self.reported_effective_wg.store(effective, Relaxed);
    }

    pub fn apply_thermal_throttle(
        &self,
        max_temp_c: u32,
        throttle_wg: u32,
        thermal_file: &str,
        gpu_index: u32,
    ) -> bool {
        if max_temp_c == 0 {
            return false;
        }
        let Some(temp) = crate::efficiency::read_thermal_c_with_gpu(thermal_file, gpu_index) else {
            return self.throttled.load(Relaxed);
        };
        let temp_c = temp as u32;
        if temp_c >= max_temp_c {
            let wg = throttle_wg.max(1);
            self.thermal_cap_wg.store(wg, Relaxed);
            self.throttled.store(true, Relaxed);
            return true;
        }
        if self.throttled.load(Relaxed) && temp_c + 5 < max_temp_c {
            self.thermal_cap_wg.store(0, Relaxed);
            self.throttled.store(false, Relaxed);
            println!(
                "[efficiency] Thermal OK ({}C) — removed work_groups thermal cap",
                temp_c
            );
        }
        false
    }

    pub fn maybe_adjust_supervene(
        &self,
        eff: &EfficiencyConf,
        gpu_nonce: u64,
        cpu_nonce: u64,
    ) {
        if !eff.dynamic_supervene || eff.supervene_max == 0 {
            return;
        }
        let n = self.adjust_counter.fetch_add(1, Relaxed);
        if n % 12 != 0 {
            return;
        }
        let total = gpu_nonce.saturating_add(cpu_nonce);
        if total == 0 {
            return;
        }
        let gpu_ratio = gpu_nonce as f64 / total as f64;
        let cur = self.active_cpu_assist.load(Relaxed);
        let min = eff.supervene_min.max(1);
        let max = eff.supervene_max.max(min);
        if gpu_ratio > 0.90 && cur > min {
            self.active_cpu_assist.store(cur - 1, Relaxed);
        } else if gpu_ratio < 0.70 && cur < max {
            self.active_cpu_assist.store(cur + 1, Relaxed);
        }
    }
}