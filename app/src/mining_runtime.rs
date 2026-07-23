//! Process-wide mining runtime: CPU assist, thermal cap, profit pause — not per-GPU OOM.

use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicU32, AtomicU64,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};

use crate::efficiency::EfficiencyConf;

pub struct MiningRuntimeState {
    pub active_cpu_assist: AtomicU32,
    pub gpu_errors: AtomicU32,
    pub throttled: AtomicBool,
    /// Stops all mining when temperature is critical or no initial sensor is available.
    thermal_paused: AtomicBool,
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
    /// Mining/result/thermal threads that have not acknowledged shutdown yet.
    active_mining_threads: AtomicU32,
}

pub(crate) struct MiningThreadGuard {
    runtime: Arc<MiningRuntimeState>,
}

impl Drop for MiningThreadGuard {
    fn drop(&mut self) {
        self.runtime.active_mining_threads.fetch_sub(1, Release);
    }
}

impl MiningRuntimeState {
    pub fn new(_configured_workgroups: u32, active_cpu: u32) -> Arc<MiningRuntimeState> {
        Arc::new(MiningRuntimeState {
            active_cpu_assist: AtomicU32::new(active_cpu),
            gpu_errors: AtomicU32::new(0),
            throttled: AtomicBool::new(false),
            thermal_paused: AtomicBool::new(false),
            paused_unprofitable: AtomicBool::new(false),
            adjust_counter: AtomicU64::new(0),
            thermal_cap_wg: AtomicU32::new(0),
            oom_work_groups_min: AtomicU32::new(0),
            effective_work_groups_min: AtomicU32::new(0),
            reported_effective_wg: AtomicU32::new(0),
            active_mining_threads: AtomicU32::new(0),
        })
    }

    pub(crate) fn track_mining_thread(self: &Arc<Self>) -> MiningThreadGuard {
        self.active_mining_threads.fetch_add(1, AcqRel);
        MiningThreadGuard {
            runtime: self.clone(),
        }
    }

    /// Background mining threads that have not yet acknowledged shutdown.
    pub fn active_mining_threads(&self) -> u32 {
        self.active_mining_threads.load(Acquire)
    }

    pub fn record_gpu_error_event(&self) {
        self.gpu_errors.fetch_add(1, Relaxed);
    }

    pub fn thermal_pause_active(&self) -> bool {
        self.thermal_paused.load(Relaxed)
    }

    pub fn thermal_workgroups_cap(&self) -> Option<u32> {
        let cap = self.thermal_cap_wg.load(Relaxed);
        if cap == 0 { None } else { Some(cap) }
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
        if value == 0 {
            return;
        }
        // These are the MINIMUM (worst-case) work_groups reached, aggregated across
        // every GPU. A plain store would overwrite with whichever device reported
        // last, hiding a more constrained device; keep the smallest non-zero value
        // seen instead. The live/current value is tracked separately in
        // `reported_effective_wg`.
        let _ = target.fetch_update(Relaxed, Relaxed, |cur| {
            if cur == 0 || value < cur {
                Some(value)
            } else {
                None
            }
        });
    }

    /// Report per-GPU OOM work_groups; aggregates latest across devices for panel stats.
    pub fn report_gpu_workgroups(&self, oom_wg: u32, thermal_cap: Option<u32>, configured_wg: u32) {
        let effective = Self::compute_effective(oom_wg, thermal_cap, configured_wg);
        Self::store_workgroup_stat(&self.oom_work_groups_min, oom_wg);
        Self::store_workgroup_stat(&self.effective_work_groups_min, effective);
        self.reported_effective_wg.store(effective, Relaxed);
    }

    /// Resolve a thermal work_groups cap that is strictly below the current load when possible.
    ///
    /// Panel historically wrote `throttle_work_groups = work_groups`, which did not reduce load.
    /// Prefer an explicit lower `throttle_wg`; otherwise auto-halve the baseline.
    pub fn resolve_thermal_cap(
        throttle_wg: u32,
        configured_wg: u32,
        reported_effective_wg: u32,
    ) -> u32 {
        let baseline = if reported_effective_wg > 0 {
            reported_effective_wg
        } else {
            configured_wg.max(1)
        };
        let requested = if throttle_wg == 0 {
            (baseline / 2).max(1)
        } else {
            throttle_wg.max(1)
        };
        if requested < baseline {
            requested
        } else if baseline > 1 {
            (baseline / 2).max(1)
        } else {
            1
        }
    }

    pub fn apply_thermal_throttle(
        &self,
        max_temp_c: u32,
        throttle_wg: u32,
        configured_wg: u32,
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
            let reported = self.reported_effective_wg.load(Relaxed);
            let wg = Self::resolve_thermal_cap(throttle_wg, configured_wg, reported);
            let prev = self.thermal_cap_wg.load(Relaxed);
            self.thermal_cap_wg.store(wg, Relaxed);
            if !self.throttled.swap(true, Relaxed) || prev != wg {
                println!(
                    "[efficiency] Thermal {}C >= {}C — cap work_groups to {} (configured {})",
                    temp_c,
                    max_temp_c,
                    wg,
                    configured_wg.max(1)
                );
            }
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

    fn ensure_thermal_cap(&self, throttle_wg: u32, configured_wg: u32) -> (u32, bool) {
        let current = self.thermal_cap_wg.load(Relaxed);
        if current > 0 {
            return (current, false);
        }
        let reported = self.reported_effective_wg.load(Relaxed);
        let cap = Self::resolve_thermal_cap(throttle_wg, configured_wg, reported);
        self.thermal_cap_wg.store(cap, Relaxed);
        (cap, true)
    }

    fn fail_closed_thermal(&self, configured_wg: u32, reason: &str) {
        let (cap, _) = self.ensure_thermal_cap(0, configured_wg);
        self.throttled.store(true, Relaxed);
        if !self.thermal_paused.swap(true, Relaxed) {
            eprintln!(
                "[Thermal] Mining paused fail-closed: {reason}; conservative work_groups cap={cap}"
            );
        }
    }

    fn observe_thermal_temperature(
        &self,
        max_temp_c: u32,
        throttle_wg: u32,
        configured_wg: u32,
        temp_c: f32,
    ) {
        if max_temp_c == 0 {
            return;
        }
        let max_temp = max_temp_c as f32;
        let critical_temp = max_temp + 5.0;
        let recovery_temp = (max_temp - 5.0).max(1.0);

        if temp_c >= critical_temp {
            let (cap, cap_changed) = self.ensure_thermal_cap(throttle_wg, configured_wg);
            let newly_throttled = !self.throttled.swap(true, Relaxed);
            if newly_throttled || cap_changed {
                eprintln!(
                    "[Thermal] {:.1}C >= {:.1}C: cap work_groups to {}",
                    temp_c, max_temp, cap
                );
            }
            if !self.thermal_paused.swap(true, Relaxed) {
                eprintln!(
                    "[Thermal] CRITICAL {:.1}C >= {:.1}C: mining paused until <= {:.1}C",
                    temp_c, critical_temp, recovery_temp
                );
            }
            return;
        }

        if temp_c >= max_temp {
            let (cap, cap_changed) = self.ensure_thermal_cap(throttle_wg, configured_wg);
            if !self.throttled.swap(true, Relaxed) || cap_changed {
                eprintln!(
                    "[Thermal] {:.1}C >= {:.1}C: cap work_groups to {}",
                    temp_c, max_temp, cap
                );
            }
            return;
        }

        if temp_c <= recovery_temp {
            let was_paused = self.thermal_paused.swap(false, Relaxed);
            let was_throttled = self.throttled.swap(false, Relaxed);
            let old_cap = self.thermal_cap_wg.swap(0, Relaxed);
            if was_paused || was_throttled || old_cap > 0 {
                println!(
                    "[Thermal] Recovered at {:.1}C (<= {:.1}C): mining resumed, cap removed",
                    temp_c, recovery_temp
                );
            }
        }
    }

    fn observe_thermal_sensor_miss(
        &self,
        consecutive_misses: u32,
        throttle_wg: u32,
        configured_wg: u32,
    ) {
        if consecutive_misses != 3 {
            return;
        }
        let (cap, _) = self.ensure_thermal_cap(throttle_wg, configured_wg);
        self.throttled.store(true, Relaxed);
        eprintln!(
            "[Thermal] Sensor missed 3 consecutive samples; preserving safety state and capping work_groups to {cap}"
        );
    }

    pub fn maybe_adjust_supervene(&self, eff: &EfficiencyConf, gpu_nonce: u64, cpu_nonce: u64) {
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

const THERMAL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2_500);

fn thermal_monitor_should_stop(stop_flag: &Option<Arc<std::sync::atomic::AtomicBool>>) -> bool {
    stop_flag
        .as_ref()
        .map(|flag| flag.load(Relaxed))
        .unwrap_or(false)
}

fn wait_for_thermal_poll(stop_flag: &Option<Arc<std::sync::atomic::AtomicBool>>) -> bool {
    let deadline = std::time::Instant::now() + THERMAL_POLL_INTERVAL;
    loop {
        if thermal_monitor_should_stop(stop_flag) {
            return true;
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(
            deadline
                .saturating_duration_since(now)
                .min(std::time::Duration::from_millis(100)),
        );
    }
}

fn hottest_sensor_reading(sensors: &[crate::efficiency::GpuTempSensorBackend]) -> Option<f32> {
    let mut hottest: Option<f32> = None;
    for sensor in sensors {
        let temp = sensor.read_c()?;
        hottest = Some(hottest.map_or(temp, |current| current.max(temp)));
    }
    hottest
}

/// How many times to retry spawning the thermal monitor thread before giving up
/// and fail-closing. A working sensor is already detected by this point, so a
/// spawn failure is a transient OS resource problem worth retrying.
const THERMAL_SPAWN_ATTEMPTS: u32 = 4;

/// Start a vendor-specific cached GPU sensor monitor before mining workers run.
pub fn start_thermal_monitor(
    runtime: &Arc<MiningRuntimeState>,
    eff: &EfficiencyConf,
    configured_wg: u32,
    devices: &[(crate::gpu_arch::GpuVendor, u32)],
    stop_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
) {
    if eff.max_temp_c == 0 {
        return;
    }

    let mut identities = Vec::new();
    for identity in devices.iter().copied() {
        if !identities.contains(&identity) {
            identities.push(identity);
        }
    }
    if identities.is_empty() {
        runtime.fail_closed_thermal(configured_wg, "no initialized GPU identity");
        return;
    }
    if !eff.thermal_file.trim().is_empty() && identities.len() != 1 {
        runtime.fail_closed_thermal(
            configured_wg,
            "one thermal_file cannot safely identify multiple selected GPUs",
        );
        return;
    }

    let mut sensors = Vec::with_capacity(identities.len());
    let mut initial_hottest: Option<f32> = None;
    for (vendor, gpu_index) in identities {
        let thermal_file = if sensors.is_empty() {
            eff.thermal_file.as_str()
        } else {
            ""
        };
        let Some((sensor, temp)) =
            crate::efficiency::detect_gpu_temp_sensor(thermal_file, gpu_index, vendor)
        else {
            let supported = match vendor {
                crate::gpu_arch::GpuVendor::Amd => "amd-smi, rocm-smi, or thermal_file",
                crate::gpu_arch::GpuVendor::Nvidia => "nvidia-smi or thermal_file",
                crate::gpu_arch::GpuVendor::Intel | crate::gpu_arch::GpuVendor::Unknown => {
                    "thermal_file"
                }
            };
            runtime.fail_closed_thermal(
                configured_wg,
                &format!(
                    "no exact temperature sensor for {:?} GPU {}; supported: {}",
                    vendor, gpu_index, supported
                ),
            );
            return;
        };
        println!(
            "[Thermal] Monitoring {:?} GPU {} with {} (initial {:.1}C)",
            vendor,
            gpu_index,
            sensor.label(),
            temp
        );
        initial_hottest = Some(initial_hottest.map_or(temp, |current| current.max(temp)));
        sensors.push(sensor);
    }

    runtime.observe_thermal_temperature(
        eff.max_temp_c,
        eff.throttle_workgroups,
        configured_wg,
        initial_hottest.unwrap_or(eff.max_temp_c as f32 + 5.0),
    );

    let max_temp_c = eff.max_temp_c;
    let throttle_wg = eff.throttle_workgroups;
    // Share the sensors across retry attempts without moving them into a spawn
    // that might fail (which would consume them).
    let sensors = std::sync::Arc::new(sensors);

    let mut spawned = false;
    for attempt in 1..=THERMAL_SPAWN_ATTEMPTS {
        let monitor_runtime = runtime.clone();
        let guard_runtime = runtime.clone();
        let sensors = sensors.clone();
        let stop_flag = stop_flag.clone();
        let spawn_result = std::thread::Builder::new()
            .name("hac-thermal-monitor".to_string())
            .spawn(move || {
                let _monitor_guard = guard_runtime.track_mining_thread();
                let mut consecutive_misses = 0u32;
                loop {
                    if wait_for_thermal_poll(&stop_flag) {
                        return;
                    }
                    match hottest_sensor_reading(&sensors) {
                        Some(temp) => {
                            if consecutive_misses > 0 {
                                println!(
                                    "[Thermal] Sensor recovered after {} missed sample(s)",
                                    consecutive_misses
                                );
                            }
                            consecutive_misses = 0;
                            monitor_runtime.observe_thermal_temperature(
                                max_temp_c,
                                throttle_wg,
                                configured_wg,
                                temp,
                            );
                        }
                        None => {
                            consecutive_misses = consecutive_misses.saturating_add(1);
                            if consecutive_misses == 1 {
                                eprintln!(
                                    "[Thermal] Sensor read failed; preserving the current safety state"
                                );
                            }
                            monitor_runtime.observe_thermal_sensor_miss(
                                consecutive_misses,
                                throttle_wg,
                                configured_wg,
                            );
                        }
                    }
                }
            });
        match spawn_result {
            Ok(_) => {
                spawned = true;
                break;
            }
            Err(error) => {
                eprintln!(
                    "[Thermal] monitor spawn attempt {attempt}/{THERMAL_SPAWN_ATTEMPTS} failed: {error}"
                );
                if attempt < THERMAL_SPAWN_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(250u64 * attempt as u64));
                }
            }
        }
    }
    // Only fail-close (pause all mining) after exhausting retries — a single
    // transient spawn failure must not permanently halt a working miner.
    if !spawned {
        runtime.fail_closed_thermal(
            configured_wg,
            "cannot start thermal monitor thread after all retries",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mining_thread_guard_acknowledges_shutdown_even_on_drop() {
        let runtime = MiningRuntimeState::new(64, 0);
        let guard = runtime.track_mining_thread();
        assert_eq!(runtime.active_mining_threads(), 1);
        drop(guard);
        assert_eq!(runtime.active_mining_threads(), 0);
    }

    #[test]
    fn thermal_cap_halves_when_throttle_equals_full_load() {
        // Panel used to write throttle_work_groups = work_groups (no real reduction).
        assert_eq!(MiningRuntimeState::resolve_thermal_cap(64, 64, 64), 32);
        assert_eq!(MiningRuntimeState::resolve_thermal_cap(1536, 1536, 0), 768);
    }

    #[test]
    fn thermal_cap_honors_explicit_lower_throttle() {
        assert_eq!(MiningRuntimeState::resolve_thermal_cap(32, 128, 128), 32);
        assert_eq!(MiningRuntimeState::resolve_thermal_cap(0, 100, 100), 50);
    }

    #[test]
    fn thermal_cap_floor_is_one() {
        assert_eq!(MiningRuntimeState::resolve_thermal_cap(1, 1, 1), 1);
    }

    #[test]
    fn thermal_cap_uses_actual_oom_reduced_workgroups() {
        assert_eq!(MiningRuntimeState::resolve_thermal_cap(64, 64, 32), 16);
        assert_eq!(MiningRuntimeState::resolve_thermal_cap(16, 64, 32), 16);
    }

    #[test]
    fn critical_temperature_pauses_until_hysteresis_recovery() {
        let runtime = MiningRuntimeState::new(64, 0);
        runtime.reported_effective_wg.store(32, Relaxed);

        runtime.observe_thermal_temperature(80, 64, 64, 80.0);
        assert_eq!(runtime.thermal_workgroups_cap(), Some(16));
        assert!(runtime.throttled.load(Relaxed));
        assert!(!runtime.thermal_pause_active());

        runtime.observe_thermal_temperature(80, 64, 64, 85.0);
        assert!(runtime.thermal_pause_active());
        runtime.observe_thermal_temperature(80, 64, 64, 76.0);
        assert!(runtime.thermal_pause_active());

        runtime.observe_thermal_temperature(80, 64, 64, 75.0);
        assert!(!runtime.thermal_pause_active());
        assert!(!runtime.throttled.load(Relaxed));
        assert_eq!(runtime.thermal_workgroups_cap(), None);
    }

    #[test]
    fn three_sensor_misses_apply_one_conservative_cap() {
        let runtime = MiningRuntimeState::new(64, 0);
        runtime.reported_effective_wg.store(40, Relaxed);

        runtime.observe_thermal_sensor_miss(1, 64, 64);
        runtime.observe_thermal_sensor_miss(2, 64, 64);
        assert_eq!(runtime.thermal_workgroups_cap(), None);

        runtime.observe_thermal_sensor_miss(3, 64, 64);
        assert_eq!(runtime.thermal_workgroups_cap(), Some(20));
        assert!(runtime.throttled.load(Relaxed));
        assert!(!runtime.thermal_pause_active());

        runtime.observe_thermal_sensor_miss(4, 64, 64);
        assert_eq!(runtime.thermal_workgroups_cap(), Some(20));
    }

    #[test]
    fn missing_initial_sensor_is_fail_closed() {
        let runtime = MiningRuntimeState::new(64, 0);
        let mut efficiency = EfficiencyConf::from_ini(&sys::IniObj::new());
        efficiency.max_temp_c = 80;
        start_thermal_monitor(&runtime, &efficiency, 64, &[], None);
        assert!(runtime.thermal_pause_active());
        assert!(runtime.throttled.load(Relaxed));
        assert_eq!(runtime.thermal_workgroups_cap(), Some(32));
    }

    #[test]
    fn one_thermal_file_cannot_claim_multi_gpu_coverage() {
        let runtime = MiningRuntimeState::new(64, 0);
        let mut efficiency = EfficiencyConf::from_ini(&sys::IniObj::new());
        efficiency.max_temp_c = 80;
        efficiency.thermal_file = "one-sensor.txt".to_string();
        start_thermal_monitor(
            &runtime,
            &efficiency,
            64,
            &[
                (crate::gpu_arch::GpuVendor::Amd, 0),
                (crate::gpu_arch::GpuVendor::Amd, 1),
            ],
            None,
        );
        assert!(runtime.thermal_pause_active());
    }
}
