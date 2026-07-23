use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering::*};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(not(windows))]
use std::time::{SystemTime, UNIX_EPOCH};

use basis::difficulty::rates_to_show;
use sys::{ini_must, ini_must_bool, ini_must_f64, ini_must_u64};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EfficiencyMode {
    Max,
    Profit,
    Eco,
}

impl EfficiencyMode {
    pub fn from_str(s: &str) -> EfficiencyMode {
        match s.trim().to_lowercase().as_str() {
            "eco" | "amd_eco" => EfficiencyMode::Eco,
            "profit" | "amd_profit" => EfficiencyMode::Profit,
            _ => EfficiencyMode::Max,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            EfficiencyMode::Eco => "eco",
            EfficiencyMode::Profit => "profit",
            EfficiencyMode::Max => "max",
        }
    }
}

#[derive(Clone, Debug)]
pub struct GpuTuning {
    pub profile: String,
    pub workgroups: u32,
    pub unitsize: u32,
}

#[derive(Clone, Debug)]
pub struct EfficiencyConf {
    pub mode: EfficiencyMode,
    pub power_cost_kwh: f64,
    pub gpu_watts: f64,
    pub cpu_watts_per_thread: f64,
    pub hac_price: f64,
    pub dynamic_supervene: bool,
    pub supervene_min: u32,
    pub supervene_max: u32,
    pub oom_fallback: bool,
    pub max_temp_c: u32,
    pub throttle_workgroups: u32,
    pub thermal_file: String,
    pub idle_start_hour: u32,
    pub idle_end_hour: u32,
    pub pause_if_unprofitable: bool,
    pub benchmark_seconds: u32,
    /// Fine-grained work_groups sweep after profile pick (default on when benchmark >= 60s).
    pub benchmark_fine_sweep: bool,
    /// GPU index for nvidia-smi temperature (0 = first GPU).
    pub thermal_gpu_index: u32,
    /// JSON stats for miner-panel GUI (e.g. miner-stats.json)
    pub stats_file: String,
}

impl EfficiencyConf {
    pub fn from_ini(ini: &sys::IniObj) -> EfficiencyConf {
        let sec = sys::ini_section(ini, "efficiency");
        let mode_raw = ini_must(sec, "mode", "profit");
        let supervene_max = ini_must_u64(sec, "supervene_max", 0) as u32;
        let supervene_min = ini_must_u64(sec, "supervene_min", 2) as u32;
        EfficiencyConf {
            mode: EfficiencyMode::from_str(&mode_raw),
            power_cost_kwh: ini_must_f64(sec, "power_cost_kwh", 0.15),
            gpu_watts: ini_must_f64(sec, "gpu_watts", 0.0),
            cpu_watts_per_thread: ini_must_f64(sec, "cpu_watts_per_thread", 8.0),
            hac_price: ini_must_f64(sec, "hac_price", 0.0),
            dynamic_supervene: ini_must_bool(sec, "dynamic_supervene", true),
            supervene_min: supervene_min.max(0),
            supervene_max,
            oom_fallback: ini_must_bool(sec, "oom_fallback", true),
            max_temp_c: ini_must_u64(sec, "max_temp_c", 0) as u32,
            throttle_workgroups: ini_must_u64(sec, "throttle_work_groups", 1024) as u32,
            thermal_file: ini_must(sec, "thermal_file", ""),
            idle_start_hour: ini_must_u64(sec, "idle_start_hour", 255) as u32,
            idle_end_hour: ini_must_u64(sec, "idle_end_hour", 255) as u32,
            pause_if_unprofitable: ini_must_bool(sec, "pause_if_unprofitable", false),
            benchmark_seconds: ini_must_u64(sec, "benchmark_seconds", 0) as u32,
            benchmark_fine_sweep: ini_must_bool(sec, "benchmark_fine_sweep", true),
            thermal_gpu_index: ini_must_u64(sec, "thermal_gpu_index", 0) as u32,
            stats_file: ini_must(sec, "stats_file", ""),
        }
    }

    pub fn wants_fine_sweep(&self) -> bool {
        self.benchmark_fine_sweep && self.benchmark_seconds >= 60
    }

    pub fn clamp_supervene(&self, configured: u32) -> u32 {
        if configured == 0 && self.supervene_max == 0 {
            return 0;
        }
        let mut sv = configured.max(1);
        let hi = if self.supervene_max > 0 {
            self.supervene_max
        } else {
            u32::MAX
        };
        sv = sv.min(hi);
        // Apply the floor, but never let a misconfigured min exceed the max: a
        // `supervene_min > supervene_max` must not spawn more threads than the cap.
        sv.max(self.supervene_min.min(hi))
    }

    pub fn spawn_supervene(&self, configured: u32) -> u32 {
        if self.dynamic_supervene && self.supervene_max > 0 {
            self.clamp_supervene(self.supervene_max)
        } else {
            self.clamp_supervene(configured)
        }
    }

    pub fn initial_active_supervene(&self, configured: u32) -> u32 {
        self.clamp_supervene(configured)
    }

    pub fn estimate_gpu_watts(&self, profile: &str) -> f64 {
        let board_max_watts = if self.gpu_watts > 0.0 {
            self.gpu_watts
        } else {
            match crate::gpu_arch::profile_vendor(profile) {
                crate::gpu_arch::GpuVendor::Amd => 350.0,
                crate::gpu_arch::GpuVendor::Nvidia => 350.0,
                crate::gpu_arch::GpuVendor::Intel => 225.0,
                crate::gpu_arch::GpuVendor::Unknown => 280.0,
            }
        };
        (board_max_watts * profile_power_factor(profile)).max(1.0)
    }

    /// Estimated draw for a measured tuning point. This lets Eco/Profit compare
    /// lower work-group settings instead of treating every point as full board power.
    pub fn estimate_tuning_watts(
        &self,
        profile: &str,
        workgroups: u32,
        unitsize: u32,
        max_workgroups: u32,
        max_unitsize: u32,
    ) -> f64 {
        let profile_watts = self.estimate_gpu_watts(profile);
        let max_load = (max_workgroups as f64 * max_unitsize as f64).max(1.0);
        let load = (workgroups as f64 * unitsize as f64 / max_load).clamp(0.0, 1.0);
        profile_watts * (0.45 + 0.55 * load.sqrt())
    }

    pub fn daily_power_cost_eur(&self, profile: &str, active_cpu_threads: u32) -> f64 {
        let gpu_w = self.estimate_gpu_watts(profile);
        let cpu_w = active_cpu_threads as f64 * self.cpu_watts_per_thread;
        (gpu_w + cpu_w) * 24.0 / 1000.0 * self.power_cost_kwh
    }

    pub fn hashes_per_joule(&self, hashrate: f64, profile: &str, active_cpu_threads: u32) -> f64 {
        let gpu_w = self.estimate_gpu_watts(profile);
        let cpu_w = active_cpu_threads as f64 * self.cpu_watts_per_thread;
        let watts = gpu_w + cpu_w;
        if watts <= 0.0 || !hashrate.is_finite() || hashrate <= 0.0 {
            return 0.0;
        }
        hashrate / watts
    }
}

pub fn profile_power_factor(profile: &str) -> f64 {
    if profile.ends_with("_eco") {
        0.58
    } else if profile.ends_with("_balanced") {
        0.70
    } else if profile.ends_with("_profit") {
        0.82
    } else if profile.ends_with("_performance") {
        0.92
    } else {
        1.0
    }
}

pub use crate::mining_runtime::MiningRuntimeState;

pub fn resolve_gpu_tuning(
    sec_gpu: &HashMap<String, Option<String>>,
    eff: &EfficiencyConf,
) -> GpuTuning {
    let profile_ini = ini_must(sec_gpu, "gpu_profile", "");
    let profile = if profile_ini.is_empty() {
        match eff.mode {
            EfficiencyMode::Eco => "amd_eco",
            EfficiencyMode::Profit => "amd_profit",
            EfficiencyMode::Max => "amd_performance",
        }
        .to_string()
    } else {
        profile_ini
    };

    let wg_ini = sec_gpu.get("work_groups").and_then(|v| v.as_ref());
    let us_ini = sec_gpu.get("unit_size").and_then(|v| v.as_ref());
    let mut workgroups = ini_must_u64(sec_gpu, "work_groups", 1024) as u32;
    let mut unitsize = ini_must_u64(sec_gpu, "unit_size", 128) as u32;
    // Apply profile defaults only when work_groups / unit_size are not set in ini
    // (benchmark autotune writes explicit values that must be preserved).
    if wg_ini.is_none() || us_ini.is_none() {
        let (wg, us) = profile_tuning(&profile);
        if wg_ini.is_none() {
            workgroups = wg;
        }
        if us_ini.is_none() {
            unitsize = us;
        }
    }
    GpuTuning {
        profile,
        workgroups,
        unitsize,
    }
}

/// Fixed work_groups / unit_size for named gpu_profile presets.
pub fn profile_tuning(profile: &str) -> (u32, u32) {
    match profile {
        "amd_eco" => (768, 128),
        "amd_balanced" => (1024, 128),
        "amd_profit" => (1536, 96),
        "amd_performance" => (2048, 96),
        "amd_max" => (4096, 128),
        "nvidia_eco" => (512, 128),
        "nvidia_balanced" => (1024, 128),
        "nvidia_profit" => (1280, 96),
        "nvidia_performance" => (1792, 96),
        "nvidia_max" => (3584, 128),
        "intel_eco" => (384, 96),
        "intel_balanced" => (512, 128),
        "intel_profit" => (768, 96),
        "intel_performance" => (1024, 96),
        "intel_max" => (1536, 128),
        _ => (1536, 96),
    }
}

/// Aggressiveness tier for named gpu_profile presets (0=eco .. 4=max).
pub fn profile_tier(profile: &str) -> i8 {
    match profile {
        "amd_eco" | "nvidia_eco" | "intel_eco" => 0,
        "amd_balanced" | "nvidia_balanced" | "intel_balanced" => 1,
        "amd_profit" | "nvidia_profit" | "intel_profit" => 2,
        "amd_performance" | "nvidia_performance" | "intel_performance" => 3,
        "amd_max" | "nvidia_max" | "intel_max" => 4,
        _ => 1,
    }
}

/// Minimum profile tier autotune may pick for an efficiency mode.
pub fn min_profile_tier_for_mode(mode: EfficiencyMode) -> i8 {
    match mode {
        EfficiencyMode::Eco => 0,
        EfficiencyMode::Profit => 1,
        EfficiencyMode::Max => 2,
    }
}

pub fn tier_profile_for_vendor(vendor: crate::gpu_arch::GpuVendor, tier: i8) -> &'static str {
    match vendor {
        crate::gpu_arch::GpuVendor::Nvidia => match tier {
            0 => "nvidia_eco",
            1 => "nvidia_balanced",
            2 => "nvidia_profit",
            3 => "nvidia_performance",
            _ => "nvidia_max",
        },
        crate::gpu_arch::GpuVendor::Intel => match tier {
            0 => "intel_eco",
            1 => "intel_balanced",
            2 => "intel_profit",
            3 => "intel_performance",
            _ => "intel_max",
        },
        _ => match tier {
            0 => "amd_eco",
            1 => "amd_balanced",
            2 => "amd_profit",
            3 => "amd_performance",
            _ => "amd_max",
        },
    }
}

/// Profiles to test during autotune for a given GPU vendor.
pub fn benchmark_profiles_for_vendor(
    vendor: crate::gpu_arch::GpuVendor,
) -> &'static [&'static str] {
    match vendor {
        crate::gpu_arch::GpuVendor::Nvidia => &[
            "nvidia_eco",
            "nvidia_balanced",
            "nvidia_profit",
            "nvidia_performance",
            "nvidia_max",
        ],
        crate::gpu_arch::GpuVendor::Intel => &[
            "intel_eco",
            "intel_balanced",
            "intel_profit",
            "intel_performance",
            "intel_max",
        ],
        _ => &[
            "amd_eco",
            "amd_balanced",
            "amd_profit",
            "amd_performance",
            "amd_max",
        ],
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BenchmarkPick {
    pub profile: String,
    pub workgroups: u32,
    pub unitsize: u32,
}

fn scale_tuning_axis(
    minimum: u32,
    maximum: u32,
    tier: i8,
    device_max_tier: i8,
    alignment: u32,
) -> u32 {
    let minimum = minimum.max(1).min(maximum.max(1));
    let maximum = maximum.max(minimum);
    if maximum == minimum || device_max_tier <= 0 {
        return maximum;
    }
    let tier = tier.clamp(0, device_max_tier) as u32;
    let max_tier = device_max_tier as u32;
    let span = maximum - minimum;
    let raw = minimum + span.saturating_mul(tier).div_ceil(max_tier);
    let aligned = raw
        .div_ceil(alignment.max(1))
        .saturating_mul(alignment.max(1));
    aligned.clamp(minimum, maximum)
}

/// Convert a named profile to an executable point inside the device's hard
/// limits. When every nominal profile exceeds a narrow architecture cap
/// (notably RDNA4), distribute tiers across the safe range instead of
/// collapsing Eco/Profit/Max to the same kernel launch.
pub fn bounded_profile_tuning(
    profile: &str,
    min_workgroups: u32,
    max_workgroups: u32,
    max_unitsize: u32,
    device_max_tier: i8,
) -> (u32, u32) {
    let floor_wg = min_workgroups.max(1).min(max_workgroups.max(1));
    let cap_wg = max_workgroups.max(floor_wg);
    let cap_us = max_unitsize.max(32);
    let tier = profile_tier(profile);
    let (nominal_wg, nominal_us) = profile_tuning(profile);
    let wg = if nominal_wg > cap_wg || nominal_wg < floor_wg {
        scale_tuning_axis(floor_wg, cap_wg, tier, device_max_tier, 16)
    } else {
        nominal_wg
    };
    let us = if nominal_us > cap_us || nominal_us < 32 {
        scale_tuning_axis(32, cap_us, tier, device_max_tier, 16)
    } else {
        nominal_us
    };
    (wg.clamp(floor_wg, cap_wg), us.clamp(32, cap_us))
}

impl BenchmarkPick {
    pub fn from_profile(profile: &str) -> BenchmarkPick {
        let (wg, us) = profile_tuning(profile);
        BenchmarkPick {
            profile: profile.to_string(),
            workgroups: wg,
            unitsize: us,
        }
    }
}

/// Build the exact profile points that can be executed on a device. Nominal
/// profile values are clamped before measurement and duplicate points collapse.
pub fn benchmark_candidates_for_device(
    vendor: crate::gpu_arch::GpuVendor,
    min_tier: i8,
    max_tier: i8,
    min_workgroups: u32,
    max_workgroups: u32,
    max_unitsize: u32,
) -> Vec<BenchmarkPick> {
    if max_workgroups == 0 || max_unitsize == 0 {
        return Vec::new();
    }
    let min_tier = min_tier.min(max_tier);
    let floor = min_workgroups.max(1).min(max_workgroups);
    let mut out = Vec::new();
    for profile in benchmark_profiles_for_vendor(vendor) {
        let tier = profile_tier(profile);
        if tier < min_tier || tier > max_tier {
            continue;
        }
        let (wg, us) =
            bounded_profile_tuning(profile, floor, max_workgroups, max_unitsize, max_tier);
        let pick = BenchmarkPick {
            profile: (*profile).to_string(),
            workgroups: wg,
            unitsize: us,
        };
        if !out.iter().any(|candidate: &BenchmarkPick| {
            candidate.workgroups == pick.workgroups && candidate.unitsize == pick.unitsize
        }) {
            out.push(pick);
        }
    }
    out
}

/// Candidate work_groups values for fine sweep around a base profile.
pub fn sweep_workgroup_candidates(
    base_wg: u32,
    vram_bytes: u64,
    localsize: u32,
    unitsize: u32,
) -> Vec<u32> {
    let min = (base_wg / 2).max(256);
    let max = base_wg.saturating_mul(3) / 2;
    sweep_workgroup_candidates_bounded(base_wg, vram_bytes, localsize, unitsize, min, max.max(min))
}

/// Architecture-aware fine sweep, including low-work-group GPUs such as RDNA4.
pub fn sweep_workgroup_candidates_bounded(
    base_wg: u32,
    vram_bytes: u64,
    localsize: u32,
    unitsize: u32,
    min_wg: u32,
    max_wg: u32,
) -> Vec<u32> {
    if max_wg == 0 {
        return Vec::new();
    }
    let floor = min_wg.max(1).min(max_wg);
    let base = base_wg.clamp(floor, max_wg);
    let lower = (base / 2).max(floor);
    let upper = base.saturating_mul(3).saturating_div(2).min(max_wg);
    let step = if max_wg <= 128 { 32 } else { 256 };
    let mut raw = vec![floor, lower, base, upper, max_wg];
    let mut wg = lower;
    while wg <= upper {
        raw.push(wg);
        let next = wg.saturating_add(step);
        if next <= wg {
            break;
        }
        wg = next;
    }
    let mut out = Vec::new();
    for candidate in raw {
        let clamped = if vram_bytes > 0 {
            clamp_workgroups_for_vram_with_floor(vram_bytes, localsize, unitsize, candidate, floor)
        } else {
            candidate
        }
        .clamp(floor, max_wg);
        if !out.contains(&clamped) {
            out.push(clamped);
        }
    }
    out.sort_unstable();
    out
}

/// Candidate unit_size values for fine benchmark sweep around a profile pick.
pub fn sweep_unitsize_candidates(base_us: u32, max_us: u32) -> Vec<u32> {
    let cap = max_us.max(32).min(160);
    let base = base_us.clamp(32, cap);
    let step = if cap <= 64 { 16 } else { 32 };
    let mut raw = vec![
        base.saturating_sub(step).max(32),
        base,
        base.saturating_add(step).min(cap),
    ];
    raw.sort_unstable();
    raw.dedup();
    raw.retain(|&us| us >= 32 && us <= cap);
    if raw.is_empty() {
        raw.push(base);
    }
    raw
}

static AUTOTUNE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn validate_atomic_target(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "refusing non-regular or linked output path {}",
                    path.display()
                ),
            ))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn replace_file_atomic(temp_path: &Path, target_path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    if !validate_atomic_target(target_path)? {
        return fs::rename(temp_path, target_path);
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ReplaceFileW(
            replaced_file_name: *const u16,
            replacement_file_name: *const u16,
            backup_file_name: *const u16,
            replace_flags: u32,
            exclude: *mut std::ffi::c_void,
            reserved: *mut std::ffi::c_void,
        ) -> i32;
    }

    let target: Vec<u16> = target_path.as_os_str().encode_wide().chain([0]).collect();
    let temp: Vec<u16> = temp_path.as_os_str().encode_wide().chain([0]).collect();
    let replaced = unsafe {
        ReplaceFileW(
            target.as_ptr(),
            temp.as_ptr(),
            std::ptr::null(),
            1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file_atomic(temp_path: &Path, target_path: &Path) -> std::io::Result<()> {
    validate_atomic_target(target_path)?;
    fs::rename(temp_path, target_path)
}

pub(crate) fn atomic_write_private(path: &Path, content: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "config path has no file name",
        )
    })?;

    for _ in 0..64 {
        let id = AUTOTUNE_TEMP_COUNTER.fetch_add(1, Relaxed);
        let temp_path: PathBuf = parent.join(format!(
            ".{}.autotune-{}-{}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
            id
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);

        let mut temp = match options.open(&temp_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };

        let result = (|| {
            temp.write_all(content)?;
            temp.flush()?;
            temp.sync_all()?;
            drop(temp);
            replace_file_atomic(&temp_path, path)?;
            #[cfg(not(windows))]
            fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        return result;
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not reserve a unique autotune temp file",
    ))
}

/// Patch poworker/diaworker ini after benchmark autotune.
pub fn apply_benchmark_pick(path: &str, pick: &BenchmarkPick) -> std::io::Result<()> {
    let content = fs::read_to_string(path)?;
    let mut out = String::new();
    let mut in_gpu = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_gpu = trimmed.eq_ignore_ascii_case("[gpu]");
        }
        let mut replaced = line.to_string();
        if in_gpu && trimmed.starts_with("gpu_profile") && trimmed.contains('=') {
            replaced = format!("gpu_profile = {}", pick.profile);
        } else if trimmed.starts_with("benchmark_seconds") && trimmed.contains('=') {
            replaced = "benchmark_seconds = 0".to_string();
        } else if in_gpu && trimmed.starts_with("work_groups") && trimmed.contains('=') {
            replaced = format!("work_groups = {}", pick.workgroups);
        } else if in_gpu && trimmed.starts_with("unit_size") && trimmed.contains('=') {
            replaced = format!("unit_size = {}", pick.unitsize);
        }
        out.push_str(&replaced);
        out.push('\n');
    }
    atomic_write_private(Path::new(path), out.as_bytes())?;
    println!(
        "[benchmark] Applied gpu_profile={} (work_groups={}, unit_size={}) to {}",
        pick.profile, pick.workgroups, pick.unitsize, path
    );
    Ok(())
}

pub fn apply_benchmark_to_ini(path: &str, profile: &str) -> std::io::Result<()> {
    apply_benchmark_pick(path, &BenchmarkPick::from_profile(profile))
}

pub fn estimate_vram_bytes(workgroups: u32, localsize: u32, unitsize: u32) -> u64 {
    let wg = workgroups as u64;
    let ls = localsize as u64;
    let us = unitsize as u64;
    let global_items = wg * ls;
    let global_hashes = 32 * us * global_items;
    let global_order = 4 * us * global_items;
    let best_hashes = 32 * wg;
    let best_nonces = 8 * wg;
    let stuff = 512;
    global_hashes + global_order + best_hashes + best_nonces + stuff + 64 * 1024 * 1024
}

pub fn clamp_workgroups_for_vram(
    vram_bytes: u64,
    localsize: u32,
    unitsize: u32,
    requested: u32,
) -> u32 {
    clamp_workgroups_for_vram_with_floor(vram_bytes, localsize, unitsize, requested, 256)
}

pub fn clamp_workgroups_for_vram_with_floor(
    vram_bytes: u64,
    localsize: u32,
    unitsize: u32,
    requested: u32,
    min_wg: u32,
) -> u32 {
    let floor = min_wg.max(1);
    if vram_bytes == 0 {
        return requested.max(floor);
    }
    let reserve = vram_bytes.saturating_mul(20) / 100;
    let budget = vram_bytes.saturating_sub(reserve).max(256 * 1024 * 1024);
    let mut wg = requested.max(floor);
    while wg >= floor {
        if estimate_vram_bytes(wg, localsize, unitsize) <= budget {
            return wg;
        }
        if wg == floor {
            break;
        }
        wg = (wg / 2).max(floor);
    }
    floor
}

pub fn is_within_idle_schedule(start_hour: u32, end_hour: u32) -> bool {
    if start_hour >= 24 || end_hour >= 24 {
        return true;
    }
    let hour = local_hour();
    if start_hour <= end_hour {
        hour >= start_hour && hour < end_hour
    } else {
        hour >= start_hour || hour < end_hour
    }
}

pub fn local_hour() -> u32 {
    local_hour_impl().min(23)
}

fn local_hour_impl() -> u32 {
    #[cfg(windows)]
    {
        #[repr(C)]
        struct SystemTimeWin {
            year: u16,
            month: u16,
            day_of_week: u16,
            day: u16,
            hour: u16,
            minute: u16,
            second: u16,
            milliseconds: u16,
        }
        unsafe extern "system" {
            fn GetLocalTime(lpSystemTime: *mut SystemTimeWin);
        }
        let mut st = SystemTimeWin {
            year: 0,
            month: 0,
            day_of_week: 0,
            day: 0,
            hour: 0,
            minute: 0,
            second: 0,
            milliseconds: 0,
        };
        unsafe {
            GetLocalTime(&mut st);
        }
        return st.hour as u32;
    }
    #[cfg(not(windows))]
    {
        use std::process::Command;
        if let Ok(out) = Command::new("date").arg("+%H").output() {
            if out.status.success() {
                if let Ok(h) = String::from_utf8_lossy(&out.stdout).trim().parse::<u32>() {
                    if h < 24 {
                        return h;
                    }
                }
            }
        }
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        ((secs / 3600) % 24) as u32
    }
}

/// True when mining workers should sleep (outside idle window or profit-paused).
pub fn mining_is_gated(runtime: &MiningRuntimeState, eff: &EfficiencyConf) -> bool {
    !is_within_idle_schedule(eff.idle_start_hour, eff.idle_end_hour)
        || runtime.paused_unprofitable.load(Relaxed)
        || runtime.thermal_pause_active()
}

fn valid_gpu_temp(value: f32) -> Option<f32> {
    if value.is_finite() && value > 0.0 && value < 120.0 {
        Some(value)
    } else {
        None
    }
}

fn numeric_values(text: &str) -> impl Iterator<Item = f32> + '_ {
    text.split(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
        .filter(|token| !token.is_empty())
        .filter_map(|token| token.parse::<f32>().ok())
        .filter_map(valid_gpu_temp)
}

fn parse_gpu_temperature_output(text: &str) -> Option<f32> {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();

    // amd-smi CSV output: pair each temperature column with the value below it.
    for pair in lines.windows(2) {
        let headers: Vec<String> = pair[0]
            .split(',')
            .map(|header| header.trim().to_ascii_lowercase())
            .collect();
        if headers.iter().any(|header| header.contains("temp")) {
            let values: Vec<&str> = pair[1].split(',').collect();
            let mut temperatures = Vec::new();
            for (index, header) in headers.iter().enumerate() {
                if header.contains("temp") {
                    if let Some(value) = values
                        .get(index)
                        .and_then(|cell| numeric_values(cell).last())
                    {
                        temperatures.push(value);
                    }
                }
            }
            if let Some(value) = temperatures.into_iter().reduce(f32::max) {
                return Some(value);
            }
        }
    }

    // JSON and human-readable rocm-smi/amd-smi formats. Only inspect lines
    // explicitly labelled as temperature so tool versions, clocks and power
    // values cannot be mistaken for a GPU sensor.
    text.lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("temp")
                || lower.contains("junction")
                || lower.contains("hotspot")
                || lower.contains("edge")
        })
        .filter_map(|line| numeric_values(line).last())
        .reduce(f32::max)
}

const SENSOR_CAPTURE_LIMIT: u64 = 64 * 1024;
static SENSOR_CAPTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn create_sensor_capture() -> Option<(PathBuf, fs::File)> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    for _ in 0..16 {
        let id = SENSOR_CAPTURE_COUNTER.fetch_add(1, Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hacash-thermal-sensor-{}-{}.tmp",
            std::process::id(),
            id
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path) {
            Ok(file) => return Some((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

fn terminate_sensor_command_bounded(child: &mut std::process::Child) {
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn command_stdout_with_timeout<S: AsRef<std::ffi::OsStr>>(
    cmd: &str,
    args: &[S],
    timeout: Duration,
) -> Option<Vec<u8>> {
    let (capture_path, capture_file) = create_sensor_capture()?;
    let mut command = Command::new(cmd);
    command
        .args(args)
        .stdout(Stdio::from(capture_file))
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            let _ = fs::remove_file(&capture_path);
            return None;
        }
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(40)),
            Ok(None) | Err(_) => {
                terminate_sensor_command_bounded(&mut child);
                break None;
            }
        }
    };
    let output = if status.as_ref().is_some_and(|status| status.success())
        && fs::metadata(&capture_path)
            .map(|metadata| metadata.len() <= SENSOR_CAPTURE_LIMIT)
            .unwrap_or(false)
    {
        fs::read(&capture_path).ok()
    } else {
        None
    };
    let _ = fs::remove_file(&capture_path);
    output
}

const SENSOR_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
enum GpuTempParser {
    Labelled,
    Scalar,
}

#[derive(Clone, Debug)]
enum GpuTempSensorSource {
    File(PathBuf),
    Command {
        program: &'static str,
        args: Vec<String>,
        parser: GpuTempParser,
    },
}

/// A sensor source selected once for one exact GPU and reused by the monitor.
#[derive(Clone, Debug)]
pub(crate) struct GpuTempSensorBackend {
    label: String,
    source: GpuTempSensorSource,
}

impl GpuTempSensorBackend {
    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn read_c(&self) -> Option<f32> {
        match &self.source {
            GpuTempSensorSource::File(path) => fs::read_to_string(path)
                .ok()
                .and_then(|raw| raw.trim().parse::<f32>().ok())
                .and_then(valid_gpu_temp),
            GpuTempSensorSource::Command {
                program,
                args,
                parser,
            } => {
                let output = command_stdout_with_timeout(program, args, SENSOR_COMMAND_TIMEOUT)?;
                let text = String::from_utf8_lossy(&output);
                match parser {
                    GpuTempParser::Labelled => parse_gpu_temperature_output(&text),
                    GpuTempParser::Scalar => {
                        text.trim().parse::<f32>().ok().and_then(valid_gpu_temp)
                    }
                }
            }
        }
    }
}

fn command_sensor(
    program: &'static str,
    label: String,
    args: Vec<String>,
    parser: GpuTempParser,
) -> GpuTempSensorBackend {
    GpuTempSensorBackend {
        label,
        source: GpuTempSensorSource::Command {
            program,
            args,
            parser,
        },
    }
}

fn detect_first_sensor(
    candidates: Vec<GpuTempSensorBackend>,
) -> Option<(GpuTempSensorBackend, f32)> {
    for sensor in candidates {
        if let Some(temp) = sensor.read_c() {
            return Some((sensor, temp));
        }
    }
    None
}

fn amd_sensor_candidates(gpu_index: u32) -> Vec<GpuTempSensorBackend> {
    let idx = gpu_index.to_string();
    vec![
        command_sensor(
            "rocm-smi",
            format!("rocm-smi JSON GPU {gpu_index}"),
            vec![
                "--showtemp".into(),
                "--json".into(),
                "-d".into(),
                idx.clone(),
            ],
            GpuTempParser::Labelled,
        ),
        command_sensor(
            "rocm-smi",
            format!("rocm-smi GPU {gpu_index}"),
            vec!["--showtemp".into(), "-d".into(), idx.clone()],
            GpuTempParser::Labelled,
        ),
        command_sensor(
            "rocm-smi",
            format!("rocm-smi alternate GPU {gpu_index}"),
            vec!["-d".into(), idx.clone(), "--showtemp".into()],
            GpuTempParser::Labelled,
        ),
        command_sensor(
            "amd-smi",
            format!("amd-smi metric GPU {gpu_index}"),
            vec![
                "metric".into(),
                "-g".into(),
                idx.clone(),
                "-t".into(),
                "--csv".into(),
            ],
            GpuTempParser::Labelled,
        ),
        command_sensor(
            "amd-smi",
            format!("amd-smi monitor GPU {gpu_index}"),
            vec![
                "monitor".into(),
                "-g".into(),
                idx,
                "-t".into(),
                "--csv".into(),
            ],
            GpuTempParser::Labelled,
        ),
    ]
}

fn nvidia_sensor(gpu_index: u32) -> GpuTempSensorBackend {
    command_sensor(
        "nvidia-smi",
        format!("nvidia-smi GPU {gpu_index}"),
        vec![
            "--query-gpu=temperature.gpu".into(),
            "--format=csv,noheader,nounits".into(),
            "-i".into(),
            gpu_index.to_string(),
        ],
        GpuTempParser::Scalar,
    )
}

pub(crate) fn detect_gpu_temp_sensor(
    thermal_file: &str,
    gpu_index: u32,
    vendor: crate::gpu_arch::GpuVendor,
) -> Option<(GpuTempSensorBackend, f32)> {
    if !thermal_file.trim().is_empty() {
        let sensor = GpuTempSensorBackend {
            label: format!("thermal file {}", thermal_file.trim()),
            source: GpuTempSensorSource::File(PathBuf::from(thermal_file.trim())),
        };
        return sensor.read_c().map(|temp| (sensor, temp));
    }

    match vendor {
        crate::gpu_arch::GpuVendor::Amd => detect_first_sensor(amd_sensor_candidates(gpu_index)),
        crate::gpu_arch::GpuVendor::Nvidia => {
            let sensor = nvidia_sensor(gpu_index);
            sensor.read_c().map(|temp| (sensor, temp))
        }
        crate::gpu_arch::GpuVendor::Intel | crate::gpu_arch::GpuVendor::Unknown => None,
    }
}

/// AMD GPU temperature via a supported rocm-smi / amd-smi command.
pub fn read_gpu_temp_amd_smi(gpu_index: u32) -> Option<f32> {
    detect_gpu_temp_sensor("", gpu_index, crate::gpu_arch::GpuVendor::Amd).map(|(_, temp)| temp)
}

pub fn read_gpu_temp_nvidia_smi(gpu_index: u32) -> Option<f32> {
    detect_gpu_temp_sensor("", gpu_index, crate::gpu_arch::GpuVendor::Nvidia).map(|(_, temp)| temp)
}

pub fn read_thermal_c(thermal_file: &str) -> Option<f32> {
    read_thermal_c_with_gpu(thermal_file, 0)
}

pub fn read_thermal_c_with_gpu(thermal_file: &str, gpu_index: u32) -> Option<f32> {
    if !thermal_file.trim().is_empty() {
        return detect_gpu_temp_sensor(
            thermal_file,
            gpu_index,
            crate::gpu_arch::GpuVendor::Unknown,
        )
        .map(|(_, temp)| temp);
    }
    read_gpu_temp_nvidia_smi(gpu_index).or_else(|| read_gpu_temp_amd_smi(gpu_index))
}

pub fn format_efficiency_line(
    hashrate: f64,
    hac_per_day: f64,
    network_pct: f64,
    eff: &EfficiencyConf,
    profile: &str,
    active_cpu: u32,
) -> String {
    let gpu_w = eff.estimate_gpu_watts(profile);
    let cpu_w = active_cpu as f64 * eff.cpu_watts_per_thread;
    let watts = gpu_w + cpu_w;
    let hpj = if watts > 0.0 {
        hashrate / watts / 1000.0
    } else {
        0.0
    };
    let daily_cost = eff.daily_power_cost_eur(profile, active_cpu);
    let mut line = format!(
        "{} | {:.0}W | {:.1}kH/J | {:.4}HAC/d {:.4}%",
        rates_to_show(hashrate),
        watts,
        hpj,
        hac_per_day,
        network_pct
    );
    if eff.hac_price > 0.0 {
        let revenue = hac_per_day * eff.hac_price;
        let net = revenue - daily_cost;
        line.push_str(&format!(" | net {:.2}EUR/d", net));
    } else if daily_cost > 0.0 {
        line.push_str(&format!(" | cost {:.2}EUR/d", daily_cost));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_cpu_assist_stays_disabled() {
        let mut ini = sys::IniObj::new();
        ini.insert(
            "efficiency".to_string(),
            std::collections::HashMap::from([
                ("supervene_min".to_string(), Some("0".to_string())),
                ("supervene_max".to_string(), Some("0".to_string())),
            ]),
        );
        let eff = EfficiencyConf::from_ini(&ini);
        assert_eq!(eff.initial_active_supervene(0), 0);
        assert_eq!(eff.spawn_supervene(0), 0);
    }

    #[test]
    fn eco_mode_profile_values() {
        let eff = EfficiencyConf {
            mode: EfficiencyMode::Eco,
            power_cost_kwh: 0.15,
            gpu_watts: 0.0,
            cpu_watts_per_thread: 8.0,
            hac_price: 0.0,
            dynamic_supervene: false,
            supervene_min: 2,
            supervene_max: 0,
            oom_fallback: true,
            max_temp_c: 0,
            throttle_workgroups: 1024,
            thermal_file: String::new(),
            idle_start_hour: 255,
            idle_end_hour: 255,
            pause_if_unprofitable: false,
            benchmark_seconds: 0,
            benchmark_fine_sweep: false,
            thermal_gpu_index: 0,
            stats_file: String::new(),
        };
        let sec = HashMap::new();
        let t = resolve_gpu_tuning(&sec, &eff);
        assert_eq!(t.profile, "amd_eco");
        assert_eq!(t.workgroups, 768);
    }

    #[test]
    fn vram_clamp_reduces_workgroups() {
        let wg = clamp_workgroups_for_vram(4 * 1024 * 1024 * 1024, 256, 128, 4096);
        assert!(wg < 4096);
        assert!(wg >= 256);
    }

    #[test]
    fn nvidia_profile_tuning_differs_from_amd() {
        let amd = profile_tuning("amd_profit");
        let nvidia = profile_tuning("nvidia_profit");
        assert_ne!(amd, nvidia);
    }

    #[test]
    fn sweep_generates_multiple_candidates() {
        let c = sweep_workgroup_candidates(1536, 0, 256, 96);
        assert!(c.len() >= 2);
        assert!(c.contains(&1536) || c.iter().any(|&w| (1400..=1600).contains(&w)));
    }

    #[test]
    fn rdna4_sweep_stays_inside_32_to_64() {
        let c = sweep_workgroup_candidates_bounded(64, 16 << 30, 256, 64, 32, 64);
        assert_eq!(c, vec![32, 64]);
    }

    #[test]
    fn benchmark_candidates_are_exact_and_unique() {
        let c = benchmark_candidates_for_device(crate::gpu_arch::GpuVendor::Amd, 2, 3, 32, 64, 64);
        assert_eq!(
            c,
            vec![BenchmarkPick {
                profile: "amd_profit".into(),
                workgroups: 64,
                unitsize: 64,
            }]
        );
    }

    #[test]
    fn intel_autotune_has_all_tiers() {
        let c = benchmark_candidates_for_device(
            crate::gpu_arch::GpuVendor::Intel,
            0,
            4,
            256,
            2048,
            128,
        );
        assert_eq!(c.len(), 5);
        assert!(c.iter().all(|pick| pick.profile.starts_with("intel_")));
    }

    #[test]
    fn configured_board_power_scales_by_profile_and_tuning() {
        let mut eff = EfficiencyConf::from_ini(&sys::IniObj::new());
        eff.gpu_watts = 300.0;
        assert!(eff.estimate_gpu_watts("amd_eco") < eff.estimate_gpu_watts("amd_max"));
        assert!(
            eff.estimate_tuning_watts("amd_max", 512, 64, 2048, 128)
                < eff.estimate_tuning_watts("amd_max", 2048, 128, 2048, 128)
        );
    }

    #[test]
    fn sweep_unitsize_candidates_respects_cap() {
        let c = sweep_unitsize_candidates(96, 128);
        assert!(c.contains(&96));
        assert!(c.iter().all(|&us| us <= 128));
    }

    #[test]
    fn rdna4_modes_get_distinct_capability_scaled_points() {
        let all =
            benchmark_candidates_for_device(crate::gpu_arch::GpuVendor::Amd, 0, 3, 32, 64, 64);
        assert!(all.iter().any(|p| p.workgroups == 32 && p.unitsize == 32));
        assert!(all.iter().any(|p| p.workgroups == 48 && p.unitsize == 48));
        assert!(all.iter().any(|p| p.workgroups == 64 && p.unitsize == 64));
    }

    #[test]
    fn rdna4_unitsize_sweep_includes_low_safe_points() {
        assert_eq!(sweep_unitsize_candidates(48, 64), vec![32, 48, 64]);
    }

    #[test]
    fn benchmark_config_replace_is_atomic_and_leaves_no_temp_file() {
        let id = AUTOTUNE_TEMP_COUNTER.fetch_add(1, Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "hacash-autotune-atomic-test-{}-{}",
            std::process::id(),
            id
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("poworker.config.ini");
        fs::write(
            &path,
            "[efficiency]\nbenchmark_seconds = 15\n[gpu]\ngpu_profile = amd_balanced\nwork_groups = 48\nunit_size = 48\n",
        )
        .unwrap();

        apply_benchmark_pick(
            path.to_str().unwrap(),
            &BenchmarkPick {
                profile: "amd_profit".to_string(),
                workgroups: 64,
                unitsize: 64,
            },
        )
        .unwrap();

        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.contains("benchmark_seconds = 0"));
        assert!(updated.contains("gpu_profile = amd_profit"));
        assert!(updated.contains("work_groups = 64"));
        assert!(updated.contains("unit_size = 64"));
        let entries: Vec<_> = fs::read_dir(&dir).unwrap().map(Result::unwrap).collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path(), path);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        }

        fs::remove_file(&path).unwrap();
        fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn gpu_temperature_parser_ignores_version_and_gpu_index() {
        let output = "AMD SMI 26.4.0\nGPU[0] : Temperature (Sensor edge) (C): 47.0\n";
        assert_eq!(parse_gpu_temperature_output(output), Some(47.0));
        assert_eq!(parse_gpu_temperature_output("AMD SMI 26.4.0"), None);
    }

    #[test]
    fn gpu_temperature_parser_reads_amd_smi_csv_columns() {
        let output = "gpu,gpu_temperature,memory_temperature,gfx_clock\n0,47,39,210\n";
        assert_eq!(parse_gpu_temperature_output(output), Some(47.0));
    }

    #[test]
    fn gpu_temperature_parser_uses_hottest_labelled_sensor() {
        let output = r#"{
            "card0": {
                "Temperature (Sensor edge) (C)": "52.0",
                "Temperature (Sensor junction) (C)": "66.0"
            }
        }"#;
        assert_eq!(parse_gpu_temperature_output(output), Some(66.0));
    }

    #[test]
    fn thermal_file_backend_is_selected_once_and_reused() {
        let (path, mut capture) = create_sensor_capture().unwrap();
        capture.write_all(b"44.5\n").unwrap();
        capture.flush().unwrap();
        drop(capture);

        let (sensor, initial) =
            detect_gpu_temp_sensor(path.to_str().unwrap(), 7, crate::gpu_arch::GpuVendor::Amd)
                .unwrap();
        assert_eq!(initial, 44.5);
        assert!(sensor.label().starts_with("thermal file "));

        fs::write(&path, "57.25\n").unwrap();
        assert_eq!(sensor.read_c(), Some(57.25));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn sensor_command_timeout_does_not_wait_for_reader_or_child_forever() {
        let started = Instant::now();
        #[cfg(windows)]
        let output = command_stdout_with_timeout(
            "powershell.exe",
            &["-NoProfile", "-Command", "Start-Sleep -Seconds 5"],
            Duration::from_millis(50),
        );
        #[cfg(not(windows))]
        let output =
            command_stdout_with_timeout("sh", &["-c", "sleep 5"], Duration::from_millis(50));

        assert!(output.is_none());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn max_mode_requires_at_least_profit_tier() {
        assert_eq!(min_profile_tier_for_mode(EfficiencyMode::Max), 2);
        assert!(profile_tier("amd_profit") >= min_profile_tier_for_mode(EfficiencyMode::Max));
        assert!(profile_tier("amd_eco") < min_profile_tier_for_mode(EfficiencyMode::Max));
    }
}

pub use crate::mining_stats::{
    MiningStatsSnapshot, build_diamond_mining_stats, build_mining_stats, write_mining_stats,
};

pub fn should_pause_for_profit(
    eff: &EfficiencyConf,
    hac_per_day: f64,
    profile: &str,
    active_cpu: u32,
) -> bool {
    if !eff.pause_if_unprofitable || eff.hac_price <= 0.0 {
        return false;
    }
    let revenue = hac_per_day * eff.hac_price;
    revenue < eff.daily_power_cost_eur(profile, active_cpu)
}

/// HACD profit pause: when `hac_price` is set, treat it as minimum daily EUR revenue
/// target; pause if electricity cost exceeds that (no per-diamond market price yet).
pub fn should_pause_for_diamond_profit(
    eff: &EfficiencyConf,
    profile: &str,
    active_cpu: u32,
) -> bool {
    if !eff.pause_if_unprofitable || eff.hac_price <= 0.0 {
        return false;
    }
    eff.daily_power_cost_eur(profile, active_cpu) > eff.hac_price
}
