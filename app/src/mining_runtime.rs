//! Process-wide mining runtime: CPU assist, thermal cap, profit pause - not per-GPU OOM.

use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicU32, AtomicU64,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};
use std::time::{Duration, Instant};

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
    /// Work_groups the OOM fallback ALLOWS, worst case across GPU workers
    /// (0 = no GPU has reported yet). On a card that never hit an out-of-memory
    /// batch this is the configured count, so it is never a measure of how much
    /// was lost: the loss is the gap below `configured_wg`.
    oom_allowed_work_groups_min: AtomicU32,
    /// Latest effective work_groups across GPU workers (OOM ∩ thermal ∩ configured).
    effective_work_groups_min: AtomicU32,
    /// Last writer for backward-compatible single-GPU panel reads.
    pub reported_effective_wg: AtomicU32,
    /// Mining/result/thermal threads that have not acknowledged shutdown yet.
    active_mining_threads: AtomicU32,
    /// Hottest GPU temperature the monitor really read, in hundredths of a
    /// degree Celsius. 0 means no sensor has ever answered, which is not the
    /// same fact as a cold card and must never reach the panel as a zero.
    gpu_temp_c100: AtomicU32,
    /// Wall clock of that reading, so a sensor that stops answering stops being
    /// quoted. 0 = never read.
    gpu_temp_unix_ms: AtomicU64,
    /// How old a reading may be before it stops being offered to the panel, in
    /// milliseconds. Set from the cadence the monitor really polls at AND from
    /// what a pass on this rig can cost, so the window and the rate readings can
    /// actually arrive at can never contradict each other.
    gpu_temp_fresh_ms: AtomicU64,
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
            oom_allowed_work_groups_min: AtomicU32::new(0),
            effective_work_groups_min: AtomicU32::new(0),
            reported_effective_wg: AtomicU32::new(0),
            active_mining_threads: AtomicU32::new(0),
            gpu_temp_c100: AtomicU32::new(0),
            gpu_temp_unix_ms: AtomicU64::new(0),
            // Before a monitor starts there is nothing to be stale, so the
            // narrowest cadence with a single sensor is the safe placeholder.
            gpu_temp_fresh_ms: AtomicU64::new(temp_freshness_ms(ThermalCadence::GUARDED, 1)),
        })
    }

    /// Tell the freshness window which cadence the monitor is running at, and
    /// over how many sensors, because a pass over eight of them is itself
    /// longer than the window a single sensor needs.
    fn set_temp_freshness_window(&self, cadence: ThermalCadence, sensor_count: usize) {
        self.gpu_temp_fresh_ms
            .store(temp_freshness_ms(cadence, sensor_count), Relaxed);
    }

    /// Publish one temperature the monitor actually read. Values outside the
    /// range a GPU sensor can produce are dropped rather than published, on the
    /// same rule the sensor parsers already apply.
    pub fn record_gpu_temperature(&self, temp_c: f32) {
        if !temp_c.is_finite() || temp_c <= 0.0 || temp_c >= 120.0 {
            return;
        }
        self.gpu_temp_c100
            .store((temp_c * 100.0).round() as u32, Relaxed);
        self.gpu_temp_unix_ms
            .store(crate::mining_stats::unix_ms_now(), Relaxed);
    }

    /// The GPU temperature this process measured, or `None` when no sensor has
    /// answered recently. Never 0: where there is no sensor there is no reading
    /// to report, and the panel is told exactly that.
    pub fn gpu_temp_c(&self) -> Option<f32> {
        let hundredths = self.gpu_temp_c100.load(Relaxed);
        let taken_at = self.gpu_temp_unix_ms.load(Relaxed);
        if hundredths == 0 || taken_at == 0 {
            return None;
        }
        if crate::mining_stats::unix_ms_now().saturating_sub(taken_at)
            > self.gpu_temp_fresh_ms.load(Relaxed)
        {
            return None;
        }
        Some(hundredths as f32 / 100.0)
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

    pub fn oom_allowed_work_groups(&self) -> u32 {
        self.oom_allowed_work_groups_min.load(Relaxed)
    }

    pub fn effective_work_groups(&self) -> u32 {
        self.effective_work_groups_min.load(Relaxed)
    }

    fn compute_effective(oom_allowed_wg: u32, thermal_cap: Option<u32>, configured_wg: u32) -> u32 {
        let mut effective = oom_allowed_wg.max(1);
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

    /// Report the work_groups one GPU is currently allowed to run; aggregates the
    /// worst case across devices for panel stats. `oom_allowed_wg` is what the OOM
    /// fallback permits right now, which on a healthy card is the configured count.
    pub fn report_gpu_workgroups(
        &self,
        oom_allowed_wg: u32,
        thermal_cap: Option<u32>,
        configured_wg: u32,
    ) {
        let effective = Self::compute_effective(oom_allowed_wg, thermal_cap, configured_wg);
        Self::store_workgroup_stat(&self.oom_allowed_work_groups_min, oom_allowed_wg);
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
                wlogln!(
                    "[efficiency] Thermal {}C >= {}C - cap work_groups to {} (configured {})",
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
            wlogln!(
                "[efficiency] Thermal OK ({}C) - removed work_groups thermal cap",
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
            wlogerr!(
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
                wlogerr!(
                    "[Thermal] {:.1}C >= {:.1}C: cap work_groups to {}",
                    temp_c, max_temp, cap
                );
            }
            if !self.thermal_paused.swap(true, Relaxed) {
                wlogerr!(
                    "[Thermal] CRITICAL {:.1}C >= {:.1}C: mining paused until <= {:.1}C",
                    temp_c, critical_temp, recovery_temp
                );
            }
            return;
        }

        if temp_c >= max_temp {
            let (cap, cap_changed) = self.ensure_thermal_cap(throttle_wg, configured_wg);
            if !self.throttled.swap(true, Relaxed) || cap_changed {
                wlogerr!(
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
                wlogln!(
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
        wlogerr!(
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

/// What one sensor pass costs.
///
/// A pass probes every selected GPU, one after another, and each probe is a
/// subprocess (or, with `thermal_file`, one file read). A probe that answers
/// costs milliseconds; a probe that stalls is bounded by SENSOR_COMMAND_TIMEOUT
/// (2s) plus the bounded kill of a process that ignored it, which is the 2.5s
/// per GPU used here. So a healthy 8-GPU rig finishes a pass in well under a
/// second and a sick one spends 20s in it, and every interval below is chosen
/// against that number rather than against a hoped-for one.
const SENSOR_PASS_WORST_CASE_PER_GPU: Duration = Duration::from_millis(2_500);

fn worst_case_pass(sensor_count: usize) -> Duration {
    let sensors = u32::try_from(sensor_count.max(1)).unwrap_or(u32::MAX);
    SENSOR_PASS_WORST_CASE_PER_GPU.saturating_mul(sensors)
}

/// The idle the monitor leaves between the END of one pass and the start of the
/// next, whatever that pass cost.
///
/// Before the start-to-start change the loop simply slept this long after every
/// pass. Keeping it as a floor is what stops a rig whose passes run longer than
/// its interval from polling with zero idle: back-to-back subprocess spawns,
/// forever, on a machine that is mining. On the guarded path the floor and the
/// interval are the same 2.5s, so the guarded cadence is arithmetically the one
/// that shipped before this work (pass, then 2.5s idle, always) and a guarded
/// rig cannot come out of it spending a larger share of its wall clock probing
/// sensors than it did going in.
const THERMAL_MIN_IDLE: Duration = Duration::from_millis(2_500);

/// Guarded cadence (`max_temp_c > 0`). Here the temperature is a safety input:
/// a late sample is a late pause, so the target stays at 2.5s. The interval is
/// deliberately NOT stretched to cover a slow pass; THERMAL_MIN_IDLE is what
/// bounds the duty cycle, so a rig whose sensors are slow samples as often as
/// those sensors allow and no oftener, exactly as it used to.
const THERMAL_POLL_INTERVAL: Duration = Duration::from_millis(2_500);

/// Reporting-only cadence (`max_temp_c == 0`, the shipped default). This is a
/// number on a screen, not a safety input. 30s is comfortably longer than the
/// worst-case pass above (~20s on an 8-GPU rig), keeps the duty cycle low on a
/// healthy rig (sub-second pass, then idle), and still refreshes the gauge
/// twice a minute, which is faster than a GPU changes temperature in any way an
/// operator who set no limit needs to watch.
const THERMAL_REPORTING_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How the monitor spaces one sensor pass from the next.
///
/// Two numbers, because one cannot express both halves of a duty cycle. The
/// interval is start-to-start, so a pass that runs long eats into the wait
/// instead of being added on top of it. The idle floor is end-to-start, so a
/// pass that runs longer than the interval still cannot close the gap to zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ThermalCadence {
    poll_interval: Duration,
    min_idle: Duration,
}

impl ThermalCadence {
    const GUARDED: ThermalCadence = ThermalCadence {
        poll_interval: THERMAL_POLL_INTERVAL,
        min_idle: THERMAL_MIN_IDLE,
    };

    const REPORTING: ThermalCadence = ThermalCadence {
        poll_interval: THERMAL_REPORTING_POLL_INTERVAL,
        min_idle: THERMAL_MIN_IDLE,
    };

    /// When the next pass may begin, given when this one began and ended.
    fn next_poll_after(&self, pass_started: Instant, pass_ended: Instant) -> Instant {
        (pass_started + self.poll_interval).max(pass_ended + self.min_idle)
    }

    /// The longest wall clock that can separate two published readings on a rig
    /// with this many sensors: one whole wait, which a worst-case pass pushes
    /// out to the idle floor, and then one whole worst-case pass before the
    /// next value exists.
    fn worst_case_sample_gap(&self, sensor_count: usize) -> Duration {
        let pass = worst_case_pass(sensor_count);
        self.poll_interval
            .max(pass.saturating_add(self.min_idle))
            .saturating_add(pass)
    }
}

/// How old a reading may get before the panel is told there is none.
///
/// A gauge still showing the last temperature of a sensor that went silent
/// would be the same lie as a gauge showing 0, but a window shorter than the
/// rig's own sampling period blanks the gauge between two perfectly healthy
/// samples. So it is two whole worst-case sampling periods plus slack, and
/// never below the historical 15s.
///
/// The sampling period is the cadence AND the pass. That distinction is the
/// whole point: on a guarded rig the interval is 2.5s, but a pass over eight
/// slow-but-healthy sensors takes longer than the old fixed 15s window all by
/// itself, so the window has to be measured against the pass or a working rig
/// blanks its own gauge. This is a display window only; the safety path acts on
/// each reading as it arrives and never consults it.
fn temp_freshness_ms(cadence: ThermalCadence, sensor_count: usize) -> u64 {
    let gap = u64::try_from(cadence.worst_case_sample_gap(sensor_count).as_millis())
        .unwrap_or(u64::MAX);
    gap.saturating_mul(2).saturating_add(5_000).max(15_000)
}

/// A sensor that flaps gets at most one failure line, and later one recovery
/// line, per this window. Without it a sensor answering every other sample
/// writes a pair of lines every few seconds into a 4 MiB worker log.
const THERMAL_MISS_LOG_INTERVAL: Duration = Duration::from_secs(300);

fn thermal_monitor_should_stop(stop_flag: &Option<Arc<std::sync::atomic::AtomicBool>>) -> bool {
    stop_flag
        .as_ref()
        .map(|flag| flag.load(Relaxed))
        .unwrap_or(false)
}

fn wait_for_thermal_poll(
    deadline: Instant,
    stop_flag: &Option<Arc<std::sync::atomic::AtomicBool>>,
) -> bool {
    loop {
        if thermal_monitor_should_stop(stop_flag) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(100)),
        );
    }
}

/// Log state for one monitor thread, kept across samples so a flapping sensor
/// cannot turn into a permanent log writer.
struct ThermalSampleLog {
    consecutive_misses: u32,
    /// Whether the current miss streak actually printed a line. A streak that
    /// was rate-limited into silence gets no recovery line either, so the two
    /// always appear as a pair or not at all.
    miss_streak_logged: bool,
    last_miss_log: Option<Instant>,
}

impl ThermalSampleLog {
    fn new() -> Self {
        ThermalSampleLog {
            consecutive_misses: 0,
            miss_streak_logged: false,
            last_miss_log: None,
        }
    }

    fn should_log_miss(&mut self) -> bool {
        let now = Instant::now();
        if self
            .last_miss_log
            .is_some_and(|at| now.duration_since(at) < THERMAL_MISS_LOG_INTERVAL)
        {
            return false;
        }
        self.last_miss_log = Some(now);
        self.miss_streak_logged = true;
        true
    }
}

/// Everything one sample does, with the reading already taken. Split out from
/// the loop so the reporting-only guarantees can be tested on the real branch
/// instead of on an early return.
fn handle_thermal_sample(
    runtime: &MiningRuntimeState,
    reading: Option<f32>,
    max_temp_c: u32,
    throttle_wg: u32,
    configured_wg: u32,
    log: &mut ThermalSampleLog,
) {
    let reporting_only = max_temp_c == 0;
    match reading {
        Some(temp) => {
            if log.miss_streak_logged {
                wlogln!(
                    "[Thermal] Sensor recovered after {} missed sample(s)",
                    log.consecutive_misses
                );
                log.miss_streak_logged = false;
            }
            log.consecutive_misses = 0;
            runtime.record_gpu_temperature(temp);
            runtime.observe_thermal_temperature(max_temp_c, throttle_wg, configured_wg, temp);
        }
        None => {
            log.consecutive_misses = log.consecutive_misses.saturating_add(1);
            if log.consecutive_misses == 1 && log.should_log_miss() {
                if reporting_only {
                    wlogerr!(
                        "[Thermal] Sensor read failed; the reported temperature goes stale until it answers again"
                    );
                } else {
                    wlogerr!("[Thermal] Sensor read failed; preserving the current safety state");
                }
            }
            // A miss is not a safety event where no limit was set: the reading
            // simply goes stale and stops being published.
            if !reporting_only {
                runtime.observe_thermal_sensor_miss(
                    log.consecutive_misses,
                    throttle_wg,
                    configured_wg,
                );
            }
        }
    }
}

fn run_thermal_monitor(
    runtime: &MiningRuntimeState,
    sensors: &[crate::efficiency::GpuTempSensorBackend],
    max_temp_c: u32,
    throttle_wg: u32,
    configured_wg: u32,
    cadence: ThermalCadence,
    stop_flag: &Option<Arc<std::sync::atomic::AtomicBool>>,
) {
    let mut log = ThermalSampleLog::new();
    let mut next_poll = Instant::now() + cadence.poll_interval;
    loop {
        if wait_for_thermal_poll(next_poll, stop_flag) {
            return;
        }
        // Both ends of the duty cycle, from the two clocks the pass itself
        // provides: the interval is measured from where this pass began, so a
        // slow pass is not added on top of a full wait; the idle floor is
        // measured from where it ended, so a pass slower than the interval
        // still cannot spawn its next subprocess immediately.
        let pass_started = Instant::now();
        let reading = hottest_sensor_reading(sensors);
        handle_thermal_sample(
            runtime,
            reading,
            max_temp_c,
            throttle_wg,
            configured_wg,
            &mut log,
        );
        next_poll = cadence.next_poll_after(pass_started, Instant::now());
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

/// Why no sensor answered, naming only what this platform can actually offer.
///
/// The old wording advised `amd-smi` and `rocm-smi` on every platform. On
/// Windows neither of those can exist (ROCm is a Linux stack and amd-smi is not
/// part of the Adrenalin driver) and neither can the Linux hwmon `thermal_file`,
/// so the advice sent every Windows operator after tools they could not install.
fn no_sensor_reason(vendor: crate::gpu_arch::GpuVendor, gpu_index: u32) -> String {
    let supported = match vendor {
        #[cfg(windows)]
        crate::gpu_arch::GpuVendor::Amd => crate::gpu_temp_adl::unavailable_reason(gpu_index),
        #[cfg(not(windows))]
        crate::gpu_arch::GpuVendor::Amd => "supported: amd-smi, rocm-smi, or thermal_file".into(),
        crate::gpu_arch::GpuVendor::Nvidia => {
            // nvidia-smi.exe is installed with the Windows driver, so unlike
            // the AMD tools it is a fair thing to ask for on either platform.
            "supported: nvidia-smi or thermal_file".to_string()
        }
        crate::gpu_arch::GpuVendor::Intel | crate::gpu_arch::GpuVendor::Unknown => {
            #[cfg(windows)]
            {
                "this platform publishes no GPU temperature for that vendor".to_string()
            }
            #[cfg(not(windows))]
            {
                "supported: thermal_file".to_string()
            }
        }
    };
    format!("no exact temperature sensor for {vendor:?} GPU {gpu_index}; {supported}")
}

/// Probe one sensor per GPU identity and return them with the hottest initial
/// reading, or the reason there is no usable sensor.
///
/// Every probe here is a subprocess (AMD tries five candidates), so on the
/// guarded path this runs on the caller before mining starts - the first
/// reading is a safety precondition there - and on the reporting-only path it
/// runs on the monitor thread instead, where nothing waits for it.
fn detect_thermal_sensors(
    thermal_file: &str,
    identities: &[(crate::gpu_arch::GpuVendor, u32)],
) -> Result<(Vec<crate::efficiency::GpuTempSensorBackend>, f32), String> {
    let mut sensors = Vec::with_capacity(identities.len());
    let mut hottest: Option<f32> = None;
    for &(vendor, gpu_index) in identities {
        // The single configured thermal_file describes the first identity only;
        // the caller has already refused to let it stand in for several GPUs.
        let file = if sensors.is_empty() { thermal_file } else { "" };
        let Some((sensor, temp)) = crate::efficiency::detect_gpu_temp_sensor(file, gpu_index, vendor)
        else {
            return Err(no_sensor_reason(vendor, gpu_index));
        };
        wlogln!(
            "[Thermal] Monitoring {:?} GPU {} with {} (initial {:.1}C)",
            vendor,
            gpu_index,
            sensor.label(),
            temp
        );
        hottest = Some(hottest.map_or(temp, |current: f32| current.max(temp)));
        sensors.push(sensor);
    }
    match hottest {
        Some(temp) => Ok((sensors, temp)),
        None => Err("no initialized GPU identity".to_string()),
    }
}

/// Reporting-only monitor: sensor detection AND polling both happen on the
/// monitor thread, so the caller reaches its mining threads immediately and the
/// first temperature simply arrives a moment later. `configured_wg` is not
/// passed in at all: with no limit set there is nothing here that may cap.
fn spawn_reporting_only_thermal_monitor(
    runtime: &Arc<MiningRuntimeState>,
    thermal_file: String,
    identities: Vec<(crate::gpu_arch::GpuVendor, u32)>,
    stop_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
) {
    let monitor_runtime = runtime.clone();
    let guard_runtime = runtime.clone();
    let spawn_result = std::thread::Builder::new()
        .name("hac-thermal-report".to_string())
        .spawn(move || {
            let _monitor_guard = guard_runtime.track_mining_thread();
            let (sensors, initial) = match detect_thermal_sensors(&thermal_file, &identities) {
                Ok(found) => found,
                Err(reason) => {
                    // One line, then the thread ends: an operator who turned the
                    // guard off gets an empty gauge, not a stopped miner.
                    wlogln!("[Thermal] No GPU temperature to report: {reason}");
                    return;
                }
            };
            monitor_runtime.record_gpu_temperature(initial);
            run_thermal_monitor(
                &monitor_runtime,
                &sensors,
                0,
                0,
                0,
                ThermalCadence::REPORTING,
                &stop_flag,
            );
        });
    if let Err(error) = spawn_result {
        // No retry loop with backoff here: retrying on the caller is exactly the
        // startup stall this path exists to avoid, and the only thing lost is a
        // gauge reading.
        wlogerr!("[Thermal] reporting-only monitor thread not started: {error}");
    }
}

/// Start a vendor-specific cached GPU sensor monitor before mining workers run.
///
/// With `max_temp_c = 0` there is no thermal limit to enforce, but the sensor is
/// still the only place a real GPU temperature exists and the panel has a gauge
/// for it. The monitor then runs in reporting-only mode: it publishes what it
/// reads and does nothing else. It never pauses mining, never caps work groups,
/// and gives up quietly on a machine with no sensor, because none of those are
/// safety decisions an operator who turned the guard off asked for. That mode
/// is also the shipped default, so it must cost the caller nothing: it does all
/// of its work, sensor probes included, on its own thread and returns at once.
pub fn start_thermal_monitor(
    runtime: &Arc<MiningRuntimeState>,
    eff: &EfficiencyConf,
    configured_wg: u32,
    devices: &[(crate::gpu_arch::GpuVendor, u32)],
    stop_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
) {
    let reporting_only = eff.max_temp_c == 0;

    let mut identities = Vec::new();
    for identity in devices.iter().copied() {
        if !identities.contains(&identity) {
            identities.push(identity);
        }
    }
    if identities.is_empty() {
        if !reporting_only {
            runtime.fail_closed_thermal(configured_wg, "no initialized GPU identity");
        }
        return;
    }
    if !eff.thermal_file.trim().is_empty() && identities.len() != 1 {
        if !reporting_only {
            runtime.fail_closed_thermal(
                configured_wg,
                "one thermal_file cannot safely identify multiple selected GPUs",
            );
        }
        return;
    }

    if reporting_only {
        // The gauge cadence, and with it the freshness window the panel is
        // judged by, is much slower than the guarded one. Every identity gets
        // its own sensor, so the count is what a pass will have to walk.
        runtime.set_temp_freshness_window(ThermalCadence::REPORTING, identities.len());
        spawn_reporting_only_thermal_monitor(
            runtime,
            eff.thermal_file.clone(),
            identities,
            stop_flag,
        );
        return;
    }

    runtime.set_temp_freshness_window(ThermalCadence::GUARDED, identities.len());
    // Guarded path only: the first reading is a safety precondition, so it is
    // taken here, on the caller, before any mining thread is spawned.
    let (sensors, initial_hottest) = match detect_thermal_sensors(&eff.thermal_file, &identities) {
        Ok(found) => found,
        Err(reason) => {
            runtime.fail_closed_thermal(configured_wg, &reason);
            return;
        }
    };

    runtime.record_gpu_temperature(initial_hottest);
    runtime.observe_thermal_temperature(
        eff.max_temp_c,
        eff.throttle_workgroups,
        configured_wg,
        initial_hottest,
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
                run_thermal_monitor(
                    &monitor_runtime,
                    &sensors,
                    max_temp_c,
                    throttle_wg,
                    configured_wg,
                    ThermalCadence::GUARDED,
                    &stop_flag,
                );
            });
        match spawn_result {
            Ok(_) => {
                spawned = true;
                break;
            }
            Err(error) => {
                wlogerr!(
                    "[Thermal] monitor spawn attempt {attempt}/{THERMAL_SPAWN_ATTEMPTS} failed: {error}"
                );
                if attempt < THERMAL_SPAWN_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(250u64 * attempt as u64));
                }
            }
        }
    }
    // Only fail-close (pause all mining) after exhausting retries - a single
    // transient spawn failure must not permanently halt a working miner. This
    // is the guarded path only; reporting-only returned above and never gets
    // here, so no `reporting_only` check is needed (or wanted) around it.
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
    fn a_runtime_that_never_read_a_sensor_reports_no_temperature() {
        let runtime = MiningRuntimeState::new(64, 0);
        assert_eq!(runtime.gpu_temp_c(), None);
        // Nothing a sensor cannot produce is ever published, so the panel can
        // never be handed a 0 that looks like a cold card.
        for impossible in [0.0, -5.0, 120.0, 999.0, f32::NAN] {
            runtime.record_gpu_temperature(impossible);
            assert_eq!(runtime.gpu_temp_c(), None, "{impossible}");
        }
        runtime.record_gpu_temperature(67.5);
        assert_eq!(runtime.gpu_temp_c(), Some(67.5));
    }

    #[test]
    fn a_sensor_that_went_silent_stops_being_quoted() {
        let runtime = MiningRuntimeState::new(64, 0);
        runtime.record_gpu_temperature(71.25);
        assert_eq!(runtime.gpu_temp_c(), Some(71.25));
        // Age the reading past the freshness window without touching the value:
        // the last temperature of a dead sensor is not a current temperature.
        let taken_at = runtime.gpu_temp_unix_ms.load(Relaxed);
        let window = runtime.gpu_temp_fresh_ms.load(Relaxed);
        runtime
            .gpu_temp_unix_ms
            .store(taken_at - window - 1, Relaxed);
        assert_eq!(runtime.gpu_temp_c(), None);
    }

    #[test]
    fn freshness_window_follows_the_poll_cadence() {
        // One constant that can contradict the interval is what let a 15s window
        // expire on every single sample of a slower loop.
        assert_eq!(temp_freshness_ms(ThermalCadence::GUARDED, 1), 20_000);
        assert!(
            temp_freshness_ms(ThermalCadence::REPORTING, 1)
                > 2 * THERMAL_REPORTING_POLL_INTERVAL.as_millis() as u64
        );
        let runtime = MiningRuntimeState::new(64, 0);
        assert_eq!(runtime.gpu_temp_fresh_ms.load(Relaxed), 20_000);
        runtime.set_temp_freshness_window(ThermalCadence::REPORTING, 1);
        assert_eq!(runtime.gpu_temp_fresh_ms.load(Relaxed), 70_000);
    }

    #[test]
    fn the_freshness_window_covers_a_slow_pass_on_a_guarded_multi_gpu_rig() {
        // Eight guarded GPUs answering slowly but successfully: about 1.9s of
        // subprocess each, so one healthy pass is 15.2s of wall clock.
        let healthy_but_slow_pass_ms = 8 * 1_900u64;
        assert!(
            healthy_but_slow_pass_ms > 15_000,
            "the old fixed 15s window expired inside a single healthy pass, \
             which blanked the gauge between two good samples"
        );

        let window = temp_freshness_ms(ThermalCadence::GUARDED, 8);
        assert!(
            window > 2 * healthy_but_slow_pass_ms,
            "a guarded window of {window}ms must survive two slow passes"
        );
        // And it is derived, not guessed: the window follows the sensor count
        // because the pass does.
        assert!(temp_freshness_ms(ThermalCadence::GUARDED, 8) > temp_freshness_ms(ThermalCadence::GUARDED, 1));

        let runtime = MiningRuntimeState::new(64, 0);
        runtime.set_temp_freshness_window(ThermalCadence::GUARDED, 8);
        runtime.record_gpu_temperature(74.0);
        let taken_at = runtime.gpu_temp_unix_ms.load(Relaxed);
        // A reading that is one whole slow pass old is still the current
        // reading of this rig, because this rig cannot produce one faster.
        runtime
            .gpu_temp_unix_ms
            .store(taken_at - healthy_but_slow_pass_ms, Relaxed);
        assert_eq!(runtime.gpu_temp_c(), Some(74.0));
    }

    #[test]
    fn the_guarded_rig_keeps_the_idle_it_had_before_start_to_start() {
        // The pre-fix loop slept 2.5s after every pass, however long the pass
        // was. The guarded cadence must still do exactly that: a 20s pass on an
        // 8-GPU rig may not be followed by an immediate respawn.
        let started = Instant::now();
        for pass in [
            Duration::from_millis(1),
            Duration::from_millis(400),
            Duration::from_secs(3),
            Duration::from_secs(20),
        ] {
            let ended = started + pass;
            let next = ThermalCadence::GUARDED.next_poll_after(started, ended);
            assert_eq!(
                next,
                ended + Duration::from_millis(2_500),
                "guarded pass of {pass:?} must be followed by the full 2.5s idle"
            );
            assert!(
                next.duration_since(ended) >= THERMAL_MIN_IDLE,
                "guarded pass of {pass:?} left less idle than the floor"
            );
        }
    }

    #[test]
    fn the_reporting_loop_keeps_start_to_start_and_still_cannot_reach_zero_idle() {
        let started = Instant::now();
        // The fix this is protecting: a 20s pass does not turn a 30s loop into
        // a 50s one, so the wait absorbs it.
        let slow = started + Duration::from_secs(20);
        assert_eq!(
            ThermalCadence::REPORTING.next_poll_after(started, slow),
            started + THERMAL_REPORTING_POLL_INTERVAL
        );
        // But absorbing it has a floor: a pass longer than the interval leaves
        // idle rather than spawning subprocesses back to back forever.
        let pathological = started + Duration::from_secs(45);
        let next = ThermalCadence::REPORTING.next_poll_after(started, pathological);
        assert_eq!(next, pathological + THERMAL_MIN_IDLE);
        assert!(next.duration_since(pathological) >= THERMAL_MIN_IDLE);
    }

    #[test]
    fn a_worst_case_pass_is_counted_per_gpu() {
        assert_eq!(worst_case_pass(0), SENSOR_PASS_WORST_CASE_PER_GPU);
        assert_eq!(worst_case_pass(1), SENSOR_PASS_WORST_CASE_PER_GPU);
        assert_eq!(worst_case_pass(8), Duration::from_secs(20));
        // Nothing here may overflow into a tiny window on an absurd rig.
        assert!(worst_case_pass(usize::MAX) >= Duration::from_secs(20));
        assert!(temp_freshness_ms(ThermalCadence::GUARDED, usize::MAX) >= 90_000);
    }

    #[test]
    fn reporting_only_mode_never_pauses_or_caps_anything() {
        // max_temp_c = 0 is the panel's own default: the guard is off. This
        // drives the monitor's real per-sample body (the same call the thread
        // makes every tick), not an early return, with the exact readings that
        // pause and cap a guarded miner.
        let runtime = MiningRuntimeState::new(64, 0);
        runtime.reported_effective_wg.store(32, Relaxed);
        let mut log = ThermalSampleLog::new();

        for reading in [
            Some(40.0),
            Some(79.9),
            Some(80.0),  // at the limit: guarded caps here
            Some(95.0),  // over critical: guarded pauses here
            None,        // sensor miss
            None,        // second miss
            None,        // third miss: guarded caps fail-closed here
            None,        // fourth
            Some(110.0), // recovery straight into a critical reading
        ] {
            handle_thermal_sample(&runtime, reading, 0, 64, 64, &mut log);
            assert!(!runtime.thermal_pause_active(), "{reading:?}");
            assert!(!runtime.throttled.load(Relaxed), "{reading:?}");
            assert_eq!(runtime.thermal_workgroups_cap(), None, "{reading:?}");
        }
        // The readings were still published: reporting-only reports.
        assert_eq!(runtime.gpu_temp_c(), Some(110.0));

        // The same readings on a guarded runtime do pause and cap, which is what
        // makes the assertions above a real guarantee rather than a dead branch.
        let guarded = MiningRuntimeState::new(64, 0);
        guarded.reported_effective_wg.store(32, Relaxed);
        let mut guarded_log = ThermalSampleLog::new();
        handle_thermal_sample(&guarded, Some(95.0), 80, 64, 64, &mut guarded_log);
        assert!(guarded.thermal_pause_active());
        assert_eq!(guarded.thermal_workgroups_cap(), Some(16));

        let missing = MiningRuntimeState::new(64, 0);
        missing.reported_effective_wg.store(32, Relaxed);
        let mut missing_log = ThermalSampleLog::new();
        for _ in 0..3 {
            handle_thermal_sample(&missing, None, 80, 64, 64, &mut missing_log);
        }
        assert!(missing.throttled.load(Relaxed));
        assert_eq!(missing.thermal_workgroups_cap(), Some(16));
    }

    #[test]
    fn a_flapping_sensor_does_not_fill_the_worker_log() {
        // Miss, hit, miss, hit ... used to print a failure line and a recovery
        // line every few seconds forever. One pair per rate-limit window now.
        let runtime = MiningRuntimeState::new(64, 0);
        let mut log = ThermalSampleLog::new();

        handle_thermal_sample(&runtime, None, 0, 0, 0, &mut log);
        assert!(log.miss_streak_logged, "first failure is reported");
        handle_thermal_sample(&runtime, Some(55.0), 0, 0, 0, &mut log);
        assert!(!log.miss_streak_logged, "recovery line pairs with it");

        for _ in 0..50 {
            handle_thermal_sample(&runtime, None, 0, 0, 0, &mut log);
            assert!(
                !log.miss_streak_logged,
                "later streaks stay silent inside the window"
            );
            handle_thermal_sample(&runtime, Some(55.0), 0, 0, 0, &mut log);
        }

        // Once the window has passed, one more pair is allowed through.
        log.last_miss_log = Some(Instant::now() - THERMAL_MISS_LOG_INTERVAL);
        handle_thermal_sample(&runtime, None, 0, 0, 0, &mut log);
        assert!(log.miss_streak_logged);
    }

    #[test]
    fn reporting_only_start_never_fail_closes_on_a_bad_configuration() {
        // The two early returns: nothing to monitor, and one thermal_file that
        // cannot stand for several GPUs. Both fail-close when a limit is set.
        let efficiency = EfficiencyConf::from_ini(&sys::IniObj::new());
        assert_eq!(efficiency.max_temp_c, 0);

        let runtime = MiningRuntimeState::new(64, 0);
        start_thermal_monitor(&runtime, &efficiency, 64, &[], None);

        let mut with_file = efficiency.clone();
        with_file.thermal_file = "one-sensor.txt".to_string();
        start_thermal_monitor(
            &runtime,
            &with_file,
            64,
            &[
                (crate::gpu_arch::GpuVendor::Amd, 0),
                (crate::gpu_arch::GpuVendor::Amd, 1),
            ],
            None,
        );

        // And a real identity with no sensor at all on this machine.
        start_thermal_monitor(
            &runtime,
            &efficiency,
            64,
            &[(crate::gpu_arch::GpuVendor::Unknown, 0)],
            None,
        );
        assert!(!runtime.thermal_pause_active());
        assert!(!runtime.throttled.load(Relaxed));
        assert_eq!(runtime.thermal_workgroups_cap(), None);
    }

    #[test]
    fn reporting_only_detects_and_publishes_off_the_caller_thread() {
        // The name of this test is the claim that the caller does not pay for
        // sensor probes, and the only thing that can prove it is ORDER:
        // `start_thermal_monitor` must return while there is still no reading,
        // and the reading must turn up afterwards. Asserting the published
        // value alone proves nothing, because a synchronous implementation
        // (detect on the caller, publish, then spawn) publishes exactly the
        // same value, never pauses and never caps: it passes every other
        // assertion here and fails only the one below.
        //
        // The order is decided by an OS thread start followed by a file open,
        // read and parse on one side, against a handful of instructions and a
        // return on the other, so this is not a coin flip.
        let path = std::env::temp_dir().join(format!(
            "hacash-thermal-monitor-test-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, "61.5\n").expect("write test sensor file");

        let runtime = MiningRuntimeState::new(64, 0);
        let mut efficiency = EfficiencyConf::from_ini(&sys::IniObj::new());
        efficiency.thermal_file = path.to_string_lossy().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let called_at = Instant::now();
        start_thermal_monitor(
            &runtime,
            &efficiency,
            64,
            &[(crate::gpu_arch::GpuVendor::Amd, 0)],
            Some(stop.clone()),
        );
        let returned_in = called_at.elapsed();
        let at_return = runtime.gpu_temp_c();

        let deadline = Instant::now() + Duration::from_secs(10);
        while runtime.gpu_temp_c().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Relaxed);
        let published = runtime.gpu_temp_c();
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            at_return, None,
            "the sensor had already been read when start_thermal_monitor \
             returned, so detection was on the caller after all"
        );
        assert!(
            returned_in < Duration::from_millis(250),
            "start_thermal_monitor took {returned_in:?} of the caller's time"
        );
        assert_eq!(published, Some(61.5));
        assert!(!runtime.thermal_pause_active());
        assert_eq!(runtime.thermal_workgroups_cap(), None);
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
    fn the_guarded_first_reading_and_first_safety_decision_happen_before_the_call_returns() {
        // block_mining_runtime spawns its mining threads on the line after
        // start_thermal_monitor returns. So on the guarded path everything that
        // decides whether this rig may hash at all - the probe, the publish and
        // the first observe - has to be finished by the time control comes back
        // here. Moving any of it onto the monitor thread lets the miner run
        // during the gap, and a mutation that did exactly that still passed
        // every other test in this file.
        let base = std::env::temp_dir();
        let cool = base.join(format!(
            "hacash-guarded-first-reading-cool-{}.txt",
            std::process::id()
        ));
        let critical = base.join(format!(
            "hacash-guarded-first-reading-critical-{}.txt",
            std::process::id()
        ));
        std::fs::write(&cool, "63.5\n").expect("write cool test sensor file");
        std::fs::write(&critical, "95.0\n").expect("write critical test sensor file");

        let runtime = MiningRuntimeState::new(64, 0);
        let mut efficiency = EfficiencyConf::from_ini(&sys::IniObj::new());
        efficiency.max_temp_c = 80;
        efficiency.thermal_file = cool.to_string_lossy().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        start_thermal_monitor(
            &runtime,
            &efficiency,
            64,
            &[(crate::gpu_arch::GpuVendor::Amd, 0)],
            Some(stop.clone()),
        );
        // Read on this line, not after a wait: the claim is about the instant of
        // return, so anything that arrives later is too late to count.
        let cool_at_return = runtime.gpu_temp_c();
        let cool_gated_at_return = crate::efficiency::mining_is_gated(&runtime, &efficiency);
        stop.store(true, Relaxed);

        // Same call on a rig that is already past critical: the pause must also
        // be in place at the instant of return, because there is no later.
        let hot_runtime = MiningRuntimeState::new(64, 0);
        let mut hot_efficiency = efficiency.clone();
        hot_efficiency.thermal_file = critical.to_string_lossy().to_string();
        let hot_stop = Arc::new(AtomicBool::new(false));
        start_thermal_monitor(
            &hot_runtime,
            &hot_efficiency,
            64,
            &[(crate::gpu_arch::GpuVendor::Amd, 0)],
            Some(hot_stop.clone()),
        );
        let hot_at_return = hot_runtime.gpu_temp_c();
        let hot_gated_at_return =
            crate::efficiency::mining_is_gated(&hot_runtime, &hot_efficiency);
        let hot_cap_at_return = hot_runtime.thermal_workgroups_cap();
        hot_stop.store(true, Relaxed);

        let _ = std::fs::remove_file(&cool);
        let _ = std::fs::remove_file(&critical);

        assert_eq!(
            cool_at_return,
            Some(63.5),
            "start_thermal_monitor returned with no guarded temperature yet, so \
             mining threads spawn before this rig has been read at all"
        );
        assert!(
            !cool_gated_at_return,
            "a 63.5C rig under an 80C limit must be free to mine on return"
        );
        assert_eq!(
            hot_at_return,
            Some(95.0),
            "the critical rig had not been read when start_thermal_monitor returned"
        );
        assert!(
            hot_gated_at_return,
            "a 95C rig under an 80C limit was still allowed to mine on return"
        );
        assert_eq!(hot_cap_at_return, Some(32));
    }

    #[test]
    fn a_guarded_rig_whose_sensor_is_gone_is_gated_before_the_call_returns() {
        // The uncooled-machine case: a limit is configured and there is no
        // sensor to enforce it with. mining_is_gated is the exact predicate
        // block_mining_runtime's worker loop consults, so asserting on it is
        // asserting that no hash is computed.
        let path = std::env::temp_dir().join(format!(
            "hacash-guarded-vanished-sensor-{}.txt",
            std::process::id()
        ));
        // Written and then removed: this is a sensor that went away, not a path
        // that was never plausible.
        std::fs::write(&path, "63.5\n").expect("write test sensor file");
        std::fs::remove_file(&path).expect("remove test sensor file");

        let runtime = MiningRuntimeState::new(64, 0);
        let mut efficiency = EfficiencyConf::from_ini(&sys::IniObj::new());
        efficiency.max_temp_c = 80;
        efficiency.thermal_file = path.to_string_lossy().to_string();

        // Control: the gate is open beforehand, so whatever closes it below is
        // the fail-close and not the idle schedule this machine happens to run
        // the suite in.
        assert!(
            !crate::efficiency::mining_is_gated(&runtime, &efficiency),
            "mining was already gated before the monitor started, so this test \
             would pass without any fail-close at all"
        );

        start_thermal_monitor(
            &runtime,
            &efficiency,
            64,
            &[(crate::gpu_arch::GpuVendor::Amd, 0)],
            None,
        );
        let _ = std::fs::remove_file(&path);

        assert!(
            crate::efficiency::mining_is_gated(&runtime, &efficiency),
            "a rig with an 80C limit and no sensor was cleared to mine uncooled"
        );
        assert!(runtime.thermal_pause_active());
        assert!(runtime.throttled.load(Relaxed));
        assert_eq!(runtime.thermal_workgroups_cap(), Some(32));

        // Control: a vendor with no sensor of its own and no file gets the same
        // answer, so the guarantee is about having no reading, not about files.
        let no_sensor_at_all = MiningRuntimeState::new(64, 0);
        let mut vendorless = EfficiencyConf::from_ini(&sys::IniObj::new());
        vendorless.max_temp_c = 80;
        assert!(!crate::efficiency::mining_is_gated(
            &no_sensor_at_all,
            &vendorless
        ));
        start_thermal_monitor(
            &no_sensor_at_all,
            &vendorless,
            64,
            &[(crate::gpu_arch::GpuVendor::Unknown, 0)],
            None,
        );
        assert!(crate::efficiency::mining_is_gated(
            &no_sensor_at_all,
            &vendorless
        ));
        assert_eq!(no_sensor_at_all.thermal_workgroups_cap(), Some(32));

        // Control: the SAME absent sensor with the guard turned off must not
        // gate anything. Without this the two asserts above would also pass if
        // start_thermal_monitor simply paused every rig it ever saw.
        let unguarded = MiningRuntimeState::new(64, 0);
        let mut no_limit = efficiency.clone();
        no_limit.max_temp_c = 0;
        start_thermal_monitor(
            &unguarded,
            &no_limit,
            64,
            &[(crate::gpu_arch::GpuVendor::Amd, 0)],
            None,
        );
        assert!(
            !crate::efficiency::mining_is_gated(&unguarded, &no_limit),
            "max_temp_c = 0 is the operator saying there is no limit to enforce; \
             a missing sensor is then nothing to fail closed on"
        );
        assert!(!unguarded.thermal_pause_active());
        assert_eq!(unguarded.thermal_workgroups_cap(), None);
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

    #[test]
    fn a_guarded_rig_that_heats_up_after_start_keeps_being_judged_by_the_monitor_thread() {
        // The caller-side first reading is the FIRST of them, not the only one.
        // A card that is cool at startup and cooks ten minutes in is the
        // ordinary case, and every other test in this file stops at the instant
        // start_thermal_monitor returns. That gap is real: a mutation that
        // spawned the guarded monitor thread and let it exit immediately - one
        // reading, ever, then an uncooled rig hashing forever - left all 173 of
        // them green.
        //
        // So this drives the loop through the sensor it actually polls: cool,
        // then hot, then cool again. The last leg matters as much as the first,
        // because a monitor that took exactly one more sample would satisfy
        // everything up to it.
        fn wait_for(deadline: Duration, mut done: impl FnMut() -> bool) -> bool {
            let limit = Instant::now() + deadline;
            while Instant::now() < limit {
                if done() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            done()
        }

        let path = std::env::temp_dir().join(format!(
            "hacash-guarded-heats-up-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, "63.5\n").expect("write test sensor file");

        let runtime = MiningRuntimeState::new(64, 0);
        let mut efficiency = EfficiencyConf::from_ini(&sys::IniObj::new());
        efficiency.max_temp_c = 80;
        efficiency.thermal_file = path.to_string_lossy().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        start_thermal_monitor(
            &runtime,
            &efficiency,
            64,
            &[(crate::gpu_arch::GpuVendor::Amd, 0)],
            Some(stop.clone()),
        );
        // Control: cool and free to mine at the instant of return, so the pause
        // below cannot be the startup fail-close or the idle schedule.
        let temp_at_return = runtime.gpu_temp_c();
        let gated_at_return = crate::efficiency::mining_is_gated(&runtime, &efficiency);

        std::fs::write(&path, "95.0\n").expect("heat the test sensor up");
        let paused = wait_for(Duration::from_secs(30), || runtime.thermal_pause_active());
        let hot_gated = crate::efficiency::mining_is_gated(&runtime, &efficiency);
        let hot_temp = runtime.gpu_temp_c();
        let hot_cap = runtime.thermal_workgroups_cap();

        std::fs::write(&path, "63.5\n").expect("cool the test sensor down");
        let resumed = wait_for(Duration::from_secs(30), || {
            !runtime.thermal_pause_active()
        });
        let cool_gated = crate::efficiency::mining_is_gated(&runtime, &efficiency);
        let cool_cap = runtime.thermal_workgroups_cap();

        stop.store(true, Relaxed);
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            temp_at_return,
            Some(63.5),
            "the caller-side first reading is missing, so this test could not \
             tell a later sample from the first one"
        );
        assert!(
            !gated_at_return,
            "the rig was already gated while still at 63.5C under an 80C limit"
        );

        assert!(
            paused,
            "the sensor went from 63.5C to 95.0C under an 80C limit and the \
             guarded monitor never paused mining: after start_thermal_monitor \
             returns, nothing is reading this rig"
        );
        assert!(
            hot_gated,
            "thermal_pause_active was set but mining_is_gated still cleared the \
             worker loop to hash at 95.0C"
        );
        assert_eq!(hot_temp, Some(95.0), "the 95.0C sample was never published");
        assert_eq!(hot_cap, Some(32), "no conservative work_groups cap at 95.0C");

        assert!(
            resumed,
            "the rig cooled back to 63.5C and stayed paused: the monitor took \
             one more sample and stopped, so it is not a loop"
        );
        assert!(!cool_gated);
        assert_eq!(cool_cap, None, "the thermal cap outlived the heat");
    }
}
