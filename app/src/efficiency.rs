use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::*};
use std::sync::Arc;
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
        let mut sv = configured.max(1);
        if self.supervene_max > 0 {
            sv = sv.min(self.supervene_max);
        }
        sv.max(self.supervene_min.max(1))
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
        if self.gpu_watts > 0.0 {
            return self.gpu_watts;
        }
        match profile {
            "amd_eco" => 180.0,
            "amd_balanced" => 220.0,
            "amd_profit" => 260.0,
            "amd_performance" => 300.0,
            "amd_max" => 350.0,
            "nvidia_eco" => 150.0,
            "nvidia_balanced" => 190.0,
            "nvidia_profit" => 230.0,
            "nvidia_performance" => 280.0,
            "nvidia_max" => 350.0,
            "intel_balanced" => 75.0,
            _ => 280.0,
        }
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

pub struct MiningRuntimeState {
    pub base_workgroups: AtomicU32,
    pub effective_workgroups: AtomicU32,
    pub active_cpu_assist: AtomicU32,
    pub gpu_errors: AtomicU32,
    pub throttled: AtomicBool,
    pub paused_unprofitable: AtomicBool,
    pub adjust_counter: AtomicU64,
}

impl MiningRuntimeState {
    pub fn new(workgroups: u32, active_cpu: u32) -> Arc<MiningRuntimeState> {
        let wg = workgroups.max(1);
        Arc::new(MiningRuntimeState {
            base_workgroups: AtomicU32::new(wg),
            effective_workgroups: AtomicU32::new(wg),
            active_cpu_assist: AtomicU32::new(active_cpu.max(1)),
            gpu_errors: AtomicU32::new(0),
            throttled: AtomicBool::new(false),
            paused_unprofitable: AtomicBool::new(false),
            adjust_counter: AtomicU64::new(0),
        })
    }

    pub fn workgroups(&self, configured: u32) -> u32 {
        let eff = self.effective_workgroups.load(Relaxed).max(1);
        eff.min(configured.max(1))
    }

    pub fn record_gpu_error(&self, configured: u32, oom_fallback: bool) -> u32 {
        self.gpu_errors.fetch_add(1, Relaxed);
        if !oom_fallback {
            return self.workgroups(configured);
        }
        let cur = self.effective_workgroups.load(Relaxed).max(1);
        let next = (cur / 2).max(256);
        if next < cur {
            eprintln!(
                "[efficiency] OpenCL error — reducing work_groups {} -> {}",
                cur, next
            );
            self.effective_workgroups.store(next, Relaxed);
        }
        next
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
        let Some(temp) = read_thermal_c_with_gpu(thermal_file, gpu_index) else {
            return self.throttled.load(Relaxed);
        };
        let temp_c = temp as u32;
        if temp_c >= max_temp_c {
            let wg = throttle_wg.max(256);
            self.effective_workgroups.store(wg, Relaxed);
            self.throttled.store(true, Relaxed);
            return true;
        }
        if self.throttled.load(Relaxed) && temp_c + 5 < max_temp_c {
            let base = self.base_workgroups.load(Relaxed).max(256);
            self.effective_workgroups.store(base, Relaxed);
            self.throttled.store(false, Relaxed);
            println!(
                "[efficiency] Thermal OK ({}C) — restored work_groups to {}",
                temp_c, base
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

pub fn resolve_gpu_tuning(sec_gpu: &HashMap<String, Option<String>>, eff: &EfficiencyConf) -> GpuTuning {
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
        "intel_balanced" => (512, 128),
        _ => (1536, 96),
    }
}

/// Profiles to test during autotune for a given GPU vendor.
pub fn benchmark_profiles_for_vendor(vendor: crate::gpu_arch::GpuVendor) -> &'static [&'static str] {
    match vendor {
        crate::gpu_arch::GpuVendor::Nvidia => &[
            "nvidia_eco",
            "nvidia_balanced",
            "nvidia_profit",
            "nvidia_performance",
            "nvidia_max",
        ],
        crate::gpu_arch::GpuVendor::Intel => &[
            "intel_balanced",
            "amd_eco",
            "amd_balanced",
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

#[derive(Clone, Debug)]
pub struct BenchmarkPick {
    pub profile: String,
    pub workgroups: u32,
    pub unitsize: u32,
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

/// Candidate work_groups values for fine sweep around a base profile.
pub fn sweep_workgroup_candidates(
    base_wg: u32,
    vram_bytes: u64,
    localsize: u32,
    unitsize: u32,
) -> Vec<u32> {
    let min = (base_wg / 2).max(256);
    let max = base_wg.saturating_mul(3) / 2;
    let mut wg = min;
    let mut out = Vec::new();
    while wg <= max {
        let clamped = if vram_bytes > 0 {
            clamp_workgroups_for_vram(vram_bytes, localsize, unitsize, wg)
        } else {
            wg
        };
        if clamped >= 256 && !out.contains(&clamped) {
            out.push(clamped);
        }
        wg = wg.saturating_add(256);
    }
    if out.is_empty() {
        out.push(base_wg.max(256));
    }
    out
}

/// Candidate unit_size values for fine benchmark sweep around a profile pick.
pub fn sweep_unitsize_candidates(base_us: u32, max_us: u32) -> Vec<u32> {
    let cap = max_us.max(base_us).clamp(64, 160);
    let mut raw = vec![
        base_us.saturating_sub(32).max(64),
        base_us,
        base_us.saturating_add(32).min(cap),
    ];
    raw.sort_unstable();
    raw.dedup();
    raw.retain(|&us| us >= 64 && us <= cap);
    if raw.is_empty() {
        raw.push(base_us.max(64));
    }
    raw
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
    fs::write(path, out)?;
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
    if vram_bytes == 0 {
        return requested.max(256);
    }
    let reserve = vram_bytes.saturating_mul(20) / 100;
    let budget = vram_bytes.saturating_sub(reserve).max(256 * 1024 * 1024);
    let mut wg = requested.max(256);
    while wg >= 256 {
        if estimate_vram_bytes(wg, localsize, unitsize) <= budget {
            return wg;
        }
        wg /= 2;
    }
    256
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
}

fn parse_temp_line_celsius(text: &str) -> Option<f32> {
    for token in text.split(|c: char| !c.is_ascii_digit() && c != '.' && c != '-') {
        if token.is_empty() {
            continue;
        }
        if let Ok(v) = token.parse::<f32>() {
            if v.is_finite() && v > 0.0 && v < 120.0 {
                return Some(v);
            }
        }
    }
    None
}

fn read_gpu_temp_from_cmd(cmd: &str, args: &[&str]) -> Option<f32> {
    use std::process::Command;
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_temp_line_celsius(&text)
}

/// AMD GPU temperature via rocm-smi / amd-smi when installed (Linux or Windows AMD driver tools).
pub fn read_gpu_temp_amd_smi(gpu_index: u32) -> Option<f32> {
    let idx = gpu_index.to_string();
    read_gpu_temp_from_cmd("rocm-smi", &["--showtemp", "-d", &idx])
        .or_else(|| read_gpu_temp_from_cmd("rocm-smi", &["-d", &idx, "--showtemp"]))
        .or_else(|| read_gpu_temp_from_cmd("amd-smi", &["monitor", "-g", &idx]))
        .or_else(|| read_gpu_temp_from_cmd("amd-smi", &["-g", &idx, "--showtemp"]))
}

pub fn read_gpu_temp_nvidia_smi(gpu_index: u32) -> Option<f32> {
    use std::process::Command;
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=temperature.gpu",
            "--format=csv,noheader,nounits",
            "-i",
            &gpu_index.to_string(),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let v: f32 = text.trim().parse().ok()?;
    if v.is_finite() && v > 0.0 && v < 120.0 {
        Some(v)
    } else {
        None
    }
}

pub fn read_thermal_c(thermal_file: &str) -> Option<f32> {
    read_thermal_c_with_gpu(thermal_file, 0)
}

pub fn read_thermal_c_with_gpu(thermal_file: &str, gpu_index: u32) -> Option<f32> {
    if !thermal_file.is_empty() {
        if let Ok(raw) = fs::read_to_string(thermal_file) {
            if let Ok(v) = raw.trim().parse::<f32>() {
                if v.is_finite() && v > 0.0 {
                    return Some(v);
                }
            }
        }
    }
    if let Some(t) = read_gpu_temp_nvidia_smi(gpu_index) {
        return Some(t);
    }
    if let Some(t) = read_gpu_temp_amd_smi(gpu_index) {
        return Some(t);
    }
    #[cfg(windows)]
    {
        read_thermal_wmi()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(windows)]
fn read_thermal_wmi() -> Option<f32> {
    use std::process::Command;
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-CimInstance -Namespace root/wmi -ClassName MSAcpi_ThermalZoneTemperature -ErrorAction SilentlyContinue | Select-Object -First 1).CurrentTemperature",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let raw: f32 = text.trim().parse().ok()?;
    if raw <= 0.0 {
        return None;
    }
    Some((raw / 10.0) - 273.15)
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
    fn sweep_unitsize_candidates_respects_cap() {
        let c = sweep_unitsize_candidates(96, 128);
        assert!(c.contains(&96));
        assert!(c.iter().all(|&us| us <= 128));
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct MiningStatsSnapshot {
    pub status: String,
    pub hashrate_hps: f64,
    pub hashrate_display: String,
    pub watts: f64,
    pub kh_per_j: f64,
    pub hac_per_day: f64,
    pub network_pct: f64,
    pub daily_cost_eur: f64,
    pub daily_revenue_eur: f64,
    pub daily_net_eur: f64,
    pub height: u64,
    pub gpu_profile: String,
    pub active_cpu_threads: u32,
    pub paused_unprofitable: bool,
    /// `hac` or `hacd`
    pub mining_kind: String,
    pub diamond_number: u32,
    pub diamond_best: String,
    pub updated_unix_ms: u64,
}

pub fn build_mining_stats(
    hashrate: f64,
    hac_per_day: f64,
    network_pct: f64,
    eff: &EfficiencyConf,
    profile: &str,
    active_cpu: u32,
    height: u64,
    paused: bool,
) -> MiningStatsSnapshot {
    let gpu_w = eff.estimate_gpu_watts(profile);
    let watts = gpu_w + active_cpu as f64 * eff.cpu_watts_per_thread;
    let kh_per_j = if watts > 0.0 {
        hashrate / watts / 1000.0
    } else {
        0.0
    };
    let daily_cost = eff.daily_power_cost_eur(profile, active_cpu);
    let daily_revenue = hac_per_day * eff.hac_price;
    let daily_net = daily_revenue - daily_cost;
    let status = if paused {
        "paused".to_string()
    } else if hashrate > 0.0 {
        "mining".to_string()
    } else {
        "idle".to_string()
    };
    MiningStatsSnapshot {
        status,
        hashrate_hps: hashrate,
        hashrate_display: rates_to_show(hashrate),
        watts,
        kh_per_j,
        hac_per_day,
        network_pct,
        daily_cost_eur: daily_cost,
        daily_revenue_eur: daily_revenue,
        daily_net_eur: daily_net,
        height,
        gpu_profile: profile.to_string(),
        active_cpu_threads: active_cpu,
        paused_unprofitable: paused,
        mining_kind: "hac".to_string(),
        diamond_number: 0,
        diamond_best: String::new(),
        updated_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    }
}

pub fn build_diamond_mining_stats(
    hashrate: f64,
    eff: &EfficiencyConf,
    profile: &str,
    active_cpu: u32,
    diamond_number: u32,
    diamond_best: &str,
    paused: bool,
) -> MiningStatsSnapshot {
    let gpu_w = eff.estimate_gpu_watts(profile);
    let watts = gpu_w + active_cpu as f64 * eff.cpu_watts_per_thread;
    let kh_per_j = if watts > 0.0 {
        hashrate / watts / 1000.0
    } else {
        0.0
    };
    let daily_cost = eff.daily_power_cost_eur(profile, active_cpu);
    let status = if paused {
        "paused".to_string()
    } else if hashrate > 0.0 {
        "mining".to_string()
    } else {
        "idle".to_string()
    };
    MiningStatsSnapshot {
        status,
        hashrate_hps: hashrate,
        hashrate_display: rates_to_show(hashrate),
        watts,
        kh_per_j,
        hac_per_day: 0.0,
        network_pct: 0.0,
        daily_cost_eur: daily_cost,
        daily_revenue_eur: 0.0,
        daily_net_eur: -daily_cost,
        height: diamond_number as u64,
        gpu_profile: profile.to_string(),
        active_cpu_threads: active_cpu,
        paused_unprofitable: paused,
        mining_kind: "hacd".to_string(),
        diamond_number,
        diamond_best: diamond_best.to_string(),
        updated_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    }
}

pub fn write_mining_stats(path: &str, stats: &MiningStatsSnapshot) {
    if path.is_empty() {
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(stats) {
        let _ = fs::write(path, json);
    }
}

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

