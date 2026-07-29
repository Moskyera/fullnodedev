//! Per-GPU OpenCL work_groups OOM recovery (halving + optional ramp-back) and the
//! time-based GPU quarantine shared by the OpenCL and CUDA backends.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use crate::gpu_arch::ArchLimits;

/// Minimum work_groups after OOM fallback on standard AMD/NVIDIA paths.
pub const OOM_FLOOR_WG: u32 = 512;
/// Successful GPU batches before restoring base work_groups after OOM reduction.
pub const OOM_RECOVERY_BATCHES: u32 = 16;
/// Clean batches before an experimental arch steps work_groups back up one level.
/// Much slower than [`OOM_RECOVERY_BATCHES`] because these arches went OOM at the
/// base size once already, but not "never", which cost half the card for the
/// whole session after a single transient CL_OUT_OF_RESOURCES.
pub const OOM_SLOW_RAMP_BATCHES: u32 = OOM_RECOVERY_BATCHES * 4;

/// Per-device work_groups state, lives on [`crate::opencl_gpu::OpenclGpuHandle`].
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
        // This is the architectural recovery minimum, not the largest
        // allocation that currently fits. The latter made record_error()
        // unable to reduce work-groups on otherwise roomy GPUs.
        let _ = (vram_bytes, localsize, unitsize);
        self.oom_floor_wg = limits.oom_floor_wg.min(configured.max(1));
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
            wlogerr!(
                "[efficiency] OpenCL error - reducing work_groups {} -> {} (floor={})",
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

    /// Adopt the work_groups a rebuilt context actually allocated.
    ///
    /// This also re-arms the ramp-back whenever the new size is below base. Without
    /// that, any path that drops the device to the floor WITHOUT going through
    /// `record_error` (a context rebuild, the quarantine re-probe) would leave
    /// `oom_reduced` false, `record_success` would return early forever, and the
    /// card would stay at the floor for the rest of the session.
    pub fn sync_effective(&mut self, wg: u32) {
        let clamped = wg.max(1);
        self.effective_workgroups.store(clamped, Relaxed);
        self.oom_reduced
            .store(clamped < self.base_workgroups.max(1), Relaxed);
        self.success_batches_since_oom.store(0, Relaxed);
    }

    /// One clean batch. Restores work_groups after an OOM reduction: in one step
    /// on standard arches, one level at a time on the experimental ones. The
    /// experimental arches used to have NO ramp at all, so a single transient
    /// CL_OUT_OF_RESOURCES halved throughput for the rest of the process.
    pub fn record_success(&self) {
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
        if self.oom_ramp_to_base {
            if n >= OOM_RECOVERY_BATCHES {
                self.effective_workgroups.store(base, Relaxed);
                self.oom_reduced.store(false, Relaxed);
                self.success_batches_since_oom.store(0, Relaxed);
                wlogln!("[efficiency] GPU stable - restored work_groups to {}", base);
            }
            return;
        }
        // Experimental arch (RDNA4): step up one level after a long clean run and
        // back off again on the next error, so 32 -> 48 -> 64 rather than 32 for
        // the whole session. The arch floor still applies on the way down.
        if n < OOM_SLOW_RAMP_BATCHES {
            return;
        }
        self.success_batches_since_oom.store(0, Relaxed);
        let next = cur.saturating_add((cur / 2).max(1)).min(base);
        if next > cur {
            self.effective_workgroups.store(next, Relaxed);
            if next >= base {
                self.oom_reduced.store(false, Relaxed);
            }
            wlogln!(
                "[efficiency] GPU stable for {} batches - raising work_groups {} -> {}",
                n, cur, next
            );
        }
    }
}

/// Consecutive failed GPU batches before a device may be quarantined.
pub const GPU_QUARANTINE_MIN_FAILURES: u32 = 20;

/// The run of failures must ALSO have lasted this long before the card is parked.
/// A failed batch costs only the bounded CPU recovery, so 20 failures on their own
/// are reached in well under a minute, which is inside a Windows TDR storm, a
/// driver update or a brief thermal excursion. Requiring both conditions keeps a
/// healthy card mining through a fast burst.
pub const GPU_QUARANTINE_MIN_ELAPSED: Duration = Duration::from_secs(120);

/// First quarantine interval. Doubles on every failed re-probe.
pub const GPU_QUARANTINE_BASE_BACKOFF: Duration = Duration::from_secs(60);

/// Longest quarantine interval. The card is re-probed at this cadence forever, so
/// a dead card costs one failed batch every half hour while a card that recovers
/// (driver reinstalled, case cooled down) resumes mining unattended.
pub const GPU_QUARANTINE_MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// How often the "still quarantined" reminder is printed for the operator.
pub const GPU_QUARANTINE_NOTICE_INTERVAL: Duration = Duration::from_secs(60);

/// Quarantine interval for a 1-based level, doubling up to the cap.
pub fn gpu_quarantine_backoff(level: u32) -> Duration {
    let shift = level.saturating_sub(1).min(31);
    let secs = GPU_QUARANTINE_BASE_BACKOFF
        .as_secs()
        .saturating_mul(1u64 << shift);
    Duration::from_secs(secs).min(GPU_QUARANTINE_MAX_BACKOFF)
}

/// Compact "90s" / "8m" / "2m 30s" for operator-facing messages.
pub fn format_backoff(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

/// Whether a batch may run right now on a device that may be quarantined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuGate {
    /// Normal operation.
    Run,
    /// The quarantine interval just expired: this batch re-probes the card.
    Reprobe { level: u32, total_failures: u64 },
    /// Still quarantined: skip the device and mine the bounded CPU recovery.
    Skip {
        level: u32,
        retry_in: Duration,
        total_failures: u64,
        /// True at most once per notice interval, so a non-technical operator can
        /// see WHY the hashrate dropped without the log being flooded.
        notify: bool,
    },
}

/// Set on the failure that armed (or re-armed) the quarantine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuQuarantineEntry {
    pub level: u32,
    pub retry_in: Duration,
}

/// Outcome of reporting one failed GPU batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuFailureReport {
    pub consecutive_failures: u32,
    pub total_failures: u64,
    /// `Some` only on the call that armed the quarantine, so the loud operator
    /// alert is printed exactly once per level.
    pub quarantined: Option<GpuQuarantineEntry>,
}

/// Read-only quarantine state for the panel / stats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuQuarantineStatus {
    pub level: u32,
    pub retry_in: Duration,
    pub total_failures: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct QuarantineInner {
    consecutive_failures: u32,
    level: u32,
    first_failure_at: Option<Instant>,
    quarantine_until: Option<Instant>,
    reprobing: bool,
    last_notice_at: Option<Instant>,
}

/// Time-based GPU quarantine shared by the OpenCL and CUDA backends.
///
/// This replaces the write-once "disable the GPU for the session" latch, which had
/// no clearing path: a card that failed 20 batches during a driver reset stayed
/// off until a human restarted the process, which on an unattended miner is a
/// total GPU-income outage for hours. Here the card is parked for a growing
/// interval and then re-probed, forever. A genuinely dead card ends up quarantined
/// almost all of the time and costs nothing; a card recovering from a 30-second
/// TDR comes back on its own while the operator sleeps.
pub struct GpuQuarantine {
    inner: Mutex<QuarantineInner>,
    total_failures: AtomicU64,
}

impl Default for GpuQuarantine {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuQuarantine {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(QuarantineInner::default()),
            total_failures: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QuarantineInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Gate one batch at `now`.
    ///
    /// Once the interval expires this opens the re-probe window and reports
    /// [`GpuGate::Reprobe`] to exactly one caller. With several GPU threads a few
    /// more may slip in beside the probe, which is bounded (one failed batch each,
    /// then the quarantine re-arms at the next level) and cannot deadlock, unlike
    /// a single-probe token that a crashing worker could leak.
    pub fn gate(&self, now: Instant) -> GpuGate {
        let mut st = self.lock();
        let total_failures = self.total_failures.load(Relaxed);
        match st.quarantine_until {
            None => GpuGate::Run,
            Some(until) if now < until => {
                let notify = match st.last_notice_at {
                    Some(last) => {
                        now.saturating_duration_since(last) >= GPU_QUARANTINE_NOTICE_INTERVAL
                    }
                    None => true,
                };
                if notify {
                    st.last_notice_at = Some(now);
                }
                GpuGate::Skip {
                    level: st.level,
                    retry_in: until.saturating_duration_since(now),
                    total_failures,
                    notify,
                }
            }
            Some(_) => {
                st.quarantine_until = None;
                st.reprobing = true;
                st.last_notice_at = Some(now);
                GpuGate::Reprobe {
                    level: st.level,
                    total_failures,
                }
            }
        }
    }

    /// Report one failed batch at `now`.
    pub fn record_failure(&self, now: Instant) -> GpuFailureReport {
        let total_failures = self.total_failures.fetch_add(1, Relaxed).saturating_add(1);
        let mut st = self.lock();
        let consecutive_failures = st.consecutive_failures.saturating_add(1);
        st.consecutive_failures = consecutive_failures;
        let first = *st.first_failure_at.get_or_insert(now);
        let persisted = now.saturating_duration_since(first);
        // A failed re-probe re-arms straight away at the next level. Otherwise the
        // run has to be long in COUNT and in TIME, so a burst cannot kill a card.
        let trip = st.reprobing
            || (consecutive_failures >= GPU_QUARANTINE_MIN_FAILURES
                && persisted >= GPU_QUARANTINE_MIN_ELAPSED);
        if !trip {
            return GpuFailureReport {
                consecutive_failures,
                total_failures,
                quarantined: None,
            };
        }
        st.reprobing = false;
        st.level = st.level.saturating_add(1);
        let retry_in = gpu_quarantine_backoff(st.level);
        st.quarantine_until = Some(now.checked_add(retry_in).unwrap_or(now));
        st.last_notice_at = Some(now);
        GpuFailureReport {
            consecutive_failures,
            total_failures,
            quarantined: Some(GpuQuarantineEntry {
                level: st.level,
                retry_in,
            }),
        }
    }

    /// Report one clean batch. Returns true when this cleared an armed quarantine
    /// or an open re-probe window, i.e. the card just came back.
    pub fn record_success(&self) -> bool {
        let mut st = self.lock();
        let recovered = st.reprobing || st.level > 0 || st.quarantine_until.is_some();
        *st = QuarantineInner::default();
        recovered
    }

    /// Current quarantine state for the panel / stats (None while mining).
    pub fn status(&self, now: Instant) -> Option<GpuQuarantineStatus> {
        let st = self.lock();
        let until = st.quarantine_until?;
        Some(GpuQuarantineStatus {
            level: st.level,
            retry_in: until.saturating_duration_since(now),
            total_failures: self.total_failures.load(Relaxed),
        })
    }

    /// One line a non-technical operator can read straight off the dashboard.
    pub fn describe(&self, now: Instant) -> String {
        match self.status(now) {
            None => "ok".to_string(),
            Some(status) => format!(
                "quarantined (level {}), retry in {}, {} failed batches",
                status.level,
                format_backoff(status.retry_in),
                status.total_failures
            ),
        }
    }

    /// Consecutive failed batches since the last clean one.
    pub fn consecutive_failures(&self) -> u32 {
        self.lock().consecutive_failures
    }

    /// Failed batches over the whole session.
    pub fn total_failures(&self) -> u64 {
        self.total_failures.load(Relaxed)
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
    fn standard_gpu_configure_floor_can_reduce_a_roomy_allocation() {
        let mut st = GpuOomState::new(2048);
        st.configure_floor(16 * 1024 * 1024 * 1024, 256, 96, 2048, "gfx1100");
        assert_eq!(st.floor_wg(), 512);
        assert_eq!(st.record_error(2048, true), 1024);
        assert_eq!(st.record_error(2048, true), 512);
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

    #[test]
    fn gfx1201_slow_ramp_recovers_from_one_transient_oom() {
        // One transient CL_OUT_OF_RESOURCES used to cost half the card for the
        // whole session, because record_success returned immediately for arches
        // with oom_ramp_to_base = false.
        let mut st = GpuOomState::new(64);
        st.configure_floor(16 * 1024 * 1024 * 1024, 256, 64, 64, "gfx1201");
        st.record_error(64, true);
        assert_eq!(st.effective_workgroups.load(Relaxed), 32);

        for _ in 0..OOM_SLOW_RAMP_BATCHES {
            st.record_success();
        }
        assert_eq!(
            st.effective_workgroups.load(Relaxed),
            48,
            "a long clean run must step gfx1201 back up one level"
        );
        for _ in 0..OOM_SLOW_RAMP_BATCHES {
            st.record_success();
        }
        assert_eq!(
            st.effective_workgroups.load(Relaxed),
            64,
            "a second clean run must reach the configured base again"
        );
        // And the arch floor still applies on the way back down.
        st.record_error(64, true);
        st.record_error(64, true);
        assert_eq!(st.effective_workgroups.load(Relaxed), 32);
    }

    #[test]
    fn a_floor_forced_by_a_rebuild_or_re_probe_can_still_ramp_back() {
        // The quarantine re-probe and the context rebuild both push the device to
        // the floor through sync_effective, not record_error. If that left the
        // ramp disarmed, a recovered card would mine at the floor forever.
        let mut st = GpuOomState::new(2048);
        st.configure_floor(16 * 1024 * 1024 * 1024, 256, 96, 2048, "gfx1100");
        st.sync_effective(st.floor_wg());
        assert_eq!(st.effective_workgroups.load(Relaxed), 512);
        for _ in 0..OOM_RECOVERY_BATCHES {
            st.record_success();
        }
        assert_eq!(
            st.effective_workgroups.load(Relaxed),
            2048,
            "a card forced to the floor must recover its configured work_groups"
        );
    }

    #[test]
    fn slow_ramp_backs_off_again_on_the_next_error() {
        let mut st = GpuOomState::new(64);
        st.configure_floor(16 * 1024 * 1024 * 1024, 256, 64, 64, "gfx1201");
        st.record_error(64, true);
        for _ in 0..OOM_SLOW_RAMP_BATCHES {
            st.record_success();
        }
        assert_eq!(st.effective_workgroups.load(Relaxed), 48);
        st.record_error(64, true);
        assert_eq!(st.effective_workgroups.load(Relaxed), 32);
    }

    /// Fail one batch every 10s until the device is parked; returns when that
    /// happened and for how long.
    fn quarantine_after_a_long_failing_run(
        q: &GpuQuarantine,
        start: Instant,
    ) -> (Instant, Duration) {
        for i in 0..GPU_QUARANTINE_MIN_FAILURES {
            let now = start + Duration::from_secs(10) * i;
            if let Some(entry) = q.record_failure(now).quarantined {
                return (now, entry.retry_in);
            }
        }
        panic!("a long, slow run of failures must quarantine the device");
    }

    #[test]
    fn a_fast_burst_of_failures_does_not_quarantine_a_healthy_card() {
        // 20 failed batches take well under a minute (a failing batch fails at
        // enqueue and the only cost is the bounded CPU recovery), which is inside
        // one Windows TDR storm or a driver update. That must not park the card.
        let q = GpuQuarantine::new();
        let start = Instant::now();
        for i in 0..GPU_QUARANTINE_MIN_FAILURES * 3 {
            let now = start + Duration::from_millis(500 * i as u64);
            let report = q.record_failure(now);
            assert!(
                report.quarantined.is_none(),
                "failure {} inside a 30s burst must not quarantine the GPU",
                report.consecutive_failures
            );
        }
        assert_eq!(q.gate(start + Duration::from_secs(31)), GpuGate::Run);
    }

    #[test]
    fn a_persistent_failing_run_quarantines_and_re_probes() {
        let q = GpuQuarantine::new();
        let start = Instant::now();
        let (armed_at, retry_in) = quarantine_after_a_long_failing_run(&q, start);
        assert_eq!(retry_in, GPU_QUARANTINE_BASE_BACKOFF);

        match q.gate(armed_at) {
            GpuGate::Skip { level, notify, .. } => {
                assert_eq!(level, 1);
                // The loud ALERT was printed by the failure that armed it, so the
                // gate stays quiet until the reminder interval is up.
                assert!(!notify);
            }
            other => panic!("expected the device to be skipped, got {:?}", other),
        }
        assert!(matches!(
            q.gate(armed_at + Duration::from_secs(1)),
            GpuGate::Skip { notify: false, .. }
        ));
        assert!(q.status(armed_at).is_some());
        assert!(q.describe(armed_at).starts_with("quarantined (level 1)"));

        // Timer expired: exactly one caller is handed the re-probe.
        let expired = armed_at + GPU_QUARANTINE_BASE_BACKOFF + Duration::from_secs(1);
        assert!(matches!(q.gate(expired), GpuGate::Reprobe { level: 1, .. }));
        assert_eq!(q.gate(expired), GpuGate::Run);

        // The probe failed, so the card is parked again at level 2 (2m). While a
        // window longer than the notice interval runs, a non-technical operator
        // gets a periodic reminder of WHY the hashrate dropped, not silence.
        assert!(q.record_failure(expired).quarantined.is_some());
        assert!(matches!(
            q.gate(expired + GPU_QUARANTINE_NOTICE_INTERVAL),
            GpuGate::Skip {
                level: 2,
                notify: true,
                ..
            }
        ));
    }

    #[test]
    fn a_failed_re_probe_doubles_the_backoff_up_to_the_cap() {
        let q = GpuQuarantine::new();
        let start = Instant::now();
        let (armed_at, _) = quarantine_after_a_long_failing_run(&q, start);

        let mut now = armed_at;
        let mut level = 1u32;
        let mut seen = vec![GPU_QUARANTINE_BASE_BACKOFF];
        for _ in 0..8 {
            now += gpu_quarantine_backoff(level) + Duration::from_secs(1);
            assert!(matches!(q.gate(now), GpuGate::Reprobe { .. }));
            // One failed probe must re-arm immediately, without waiting for
            // another 20 failures over two minutes.
            let entry = q
                .record_failure(now)
                .quarantined
                .expect("a failed re-probe must re-arm the quarantine");
            level = entry.level;
            seen.push(entry.retry_in);
        }
        assert_eq!(
            &seen[..5],
            &[
                Duration::from_secs(60),
                Duration::from_secs(120),
                Duration::from_secs(240),
                Duration::from_secs(480),
                Duration::from_secs(960),
            ]
        );
        assert_eq!(*seen.last().unwrap(), GPU_QUARANTINE_MAX_BACKOFF);
        // Never permanently disabled: the card keeps being re-probed at the cap.
        now += GPU_QUARANTINE_MAX_BACKOFF + Duration::from_secs(1);
        assert!(matches!(q.gate(now), GpuGate::Reprobe { .. }));
    }

    #[test]
    fn a_successful_re_probe_clears_the_quarantine_completely() {
        let q = GpuQuarantine::new();
        let start = Instant::now();
        let (armed_at, _) = quarantine_after_a_long_failing_run(&q, start);
        let expired = armed_at + GPU_QUARANTINE_BASE_BACKOFF + Duration::from_secs(1);
        assert!(matches!(q.gate(expired), GpuGate::Reprobe { .. }));

        assert!(
            q.record_success(),
            "recovery must be announced exactly once"
        );
        assert!(!q.record_success());
        assert_eq!(q.gate(expired), GpuGate::Run);
        assert_eq!(q.consecutive_failures(), 0);
        assert!(q.status(expired).is_none());
        assert_eq!(q.describe(expired), "ok");

        // Back to the full forgiving trigger: one later failure must not re-park.
        assert!(
            q.record_failure(expired + Duration::from_secs(1))
                .quarantined
                .is_none()
        );
    }

    #[test]
    fn total_failures_survive_recovery_for_the_operator_alert() {
        let q = GpuQuarantine::new();
        let start = Instant::now();
        quarantine_after_a_long_failing_run(&q, start);
        let before = q.total_failures();
        assert!(before >= GPU_QUARANTINE_MIN_FAILURES as u64);
        q.record_success();
        assert_eq!(
            q.total_failures(),
            before,
            "session failure total must keep counting so a flapping card is visible"
        );
    }
}
