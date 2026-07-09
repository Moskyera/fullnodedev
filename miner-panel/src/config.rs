use std::collections::HashMap;
use std::path::Path;

use app::efficiency::EfficiencyMode;

use crate::presets::{gpu_idx_for_profile, tuning_for_profile, CpuPreset, GpuPreset};

pub struct PanelSettings {
    pub cpu: CpuPreset,
    pub gpu: GpuPreset,
    pub mode: EfficiencyMode,
    pub power_cost_kwh: f64,
    pub hac_price: f64,
    pub platform_id: u32,
    pub device_id: u32,
    pub connect: String,
    pub stats_file: String,
    pub opencl_dir: String,
    pub max_temp_c: u32,
    pub pause_if_unprofitable: bool,
    pub benchmark_seconds: u32,
    pub idle_start_hour: u32,
    pub idle_end_hour: u32,
    pub benchmark_fine_sweep: bool,
    pub thermal_gpu_index: u32,
    pub work_groups: u32,
    pub unit_size: u32,
}

#[derive(Default)]
pub struct LoadedPanelIni {
    pub supervene: Option<u32>,
    pub gpu_profile: Option<String>,
    pub work_groups: Option<u32>,
    pub unit_size: Option<u32>,
    pub platform_id: Option<u32>,
    pub device_id: Option<u32>,
    pub connect: Option<String>,
    pub mode: Option<EfficiencyMode>,
    pub power_cost_kwh: Option<f64>,
    pub hac_price: Option<f64>,
    pub max_temp_c: Option<u32>,
    pub pause_if_unprofitable: Option<bool>,
}

fn parse_u32(s: &str) -> Option<u32> {
    s.trim().parse().ok()
}

fn parse_f64(s: &str) -> Option<f64> {
    s.trim().parse().ok()
}

fn section_map(content: &str, section: &str) -> HashMap<String, String> {
    let tag = format!("[{section}]");
    let mut in_section = false;
    let mut out = HashMap::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = trimmed.eq_ignore_ascii_case(&tag);
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            out.insert(k.trim().to_lowercase(), v.split(';').next().unwrap_or(v).trim().to_string());
        }
    }
    out
}

fn root_value(content: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = true;
            continue;
        }
        if in_section {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            if k.trim().eq_ignore_ascii_case(key) {
                return Some(v.split(';').next().unwrap_or(v).trim().to_string());
            }
        }
    }
    None
}

pub fn load_panel_ini(path: &Path) -> LoadedPanelIni {
    let Ok(content) = std::fs::read_to_string(path) else {
        return LoadedPanelIni::default();
    };
    let gpu = section_map(&content, "gpu");
    let eff = section_map(&content, "efficiency");
    LoadedPanelIni {
        supervene: root_value(&content, "supervene").and_then(|v| parse_u32(&v)),
        gpu_profile: gpu.get("gpu_profile").cloned(),
        work_groups: gpu.get("work_groups").and_then(|v| parse_u32(v)),
        unit_size: gpu.get("unit_size").and_then(|v| parse_u32(v)),
        platform_id: gpu.get("platform_id").and_then(|v| parse_u32(v)),
        device_id: gpu
            .get("device_ids")
            .and_then(|v| v.split(',').next())
            .and_then(|v| parse_u32(v)),
        connect: root_value(&content, "connect"),
        mode: eff
            .get("mode")
            .map(|s| EfficiencyMode::from_str(s)),
        power_cost_kwh: eff.get("power_cost_kwh").and_then(|v| parse_f64(v)),
        hac_price: eff.get("hac_price").and_then(|v| parse_f64(v)),
        max_temp_c: eff.get("max_temp_c").and_then(|v| parse_u32(v)),
        pause_if_unprofitable: eff
            .get("pause_if_unprofitable")
            .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes")),
    }
}

pub fn apply_loaded_ini(
    loaded: &LoadedPanelIni,
    cpus: &[CpuPreset],
    gpus: &[GpuPreset],
    cpu_idx: &mut usize,
    gpu_idx: &mut usize,
    mode_idx: &mut usize,
    work_groups: &mut u32,
    unit_size: &mut u32,
    platform_id: &mut u32,
    device_id: &mut u32,
    connect: &mut String,
    power_cost: &mut f32,
    hac_price: &mut f32,
    max_temp_c: &mut u32,
    pause_unprofitable: &mut bool,
) {
    if let Some(sv) = loaded.supervene {
        if let Some(i) = cpus.iter().position(|c| c.supervene == sv) {
            *cpu_idx = i;
        }
    }
    if let Some(ref profile) = loaded.gpu_profile {
        if let Some(i) = gpu_idx_for_profile(gpus, profile) {
            *gpu_idx = i;
        }
    }
    if let Some(mode) = loaded.mode {
        *mode_idx = match mode {
            EfficiencyMode::Eco => 0,
            EfficiencyMode::Max => 2,
            EfficiencyMode::Profit => 1,
        };
    }
    if let Some(wg) = loaded.work_groups {
        *work_groups = wg;
    } else if let Some(ref profile) = loaded.gpu_profile {
        let (wg, us) = tuning_for_profile(profile);
        *work_groups = wg;
        *unit_size = us;
    }
    if let Some(us) = loaded.unit_size {
        *unit_size = us;
    }
    if let Some(p) = loaded.platform_id {
        *platform_id = p;
    }
    if let Some(d) = loaded.device_id {
        *device_id = d;
    }
    if let Some(ref c) = loaded.connect {
        *connect = c.clone();
    }
    if let Some(c) = loaded.power_cost_kwh {
        *power_cost = c as f32;
    }
    if let Some(p) = loaded.hac_price {
        *hac_price = p as f32;
    }
    if let Some(t) = loaded.max_temp_c {
        *max_temp_c = t;
    }
    if let Some(p) = loaded.pause_if_unprofitable {
        *pause_unprofitable = p;
    }
}

fn efficiency_section(s: &PanelSettings) -> String {
    let mode = s.mode.label();
    format!(
        r"[efficiency]
mode = {mode}
power_cost_kwh = {cost}
gpu_watts = {gpu_watts}
cpu_watts_per_thread = 8
hac_price = {hac_price}
dynamic_supervene = true
supervene_min = 2
supervene_max = {sv}
oom_fallback = true
max_temp_c = {max_temp}
throttle_work_groups = 1024
idle_start_hour = {idle_start}
idle_end_hour = {idle_end}
pause_if_unprofitable = {pause_unprofitable}
benchmark_seconds = {benchmark_seconds}
benchmark_fine_sweep = {fine_sweep}
thermal_gpu_index = {thermal_gpu}
stats_file = {stats_file}
",
        mode = mode,
        cost = s.power_cost_kwh,
        gpu_watts = s.gpu.watts,
        hac_price = s.hac_price,
        sv = s.cpu.supervene,
        max_temp = s.max_temp_c,
        idle_start = s.idle_start_hour,
        idle_end = s.idle_end_hour,
        pause_unprofitable = s.pause_if_unprofitable,
        benchmark_seconds = s.benchmark_seconds,
        fine_sweep = s.benchmark_fine_sweep,
        thermal_gpu = s.thermal_gpu_index,
        stats_file = s.stats_file,
    )
}

pub fn write_poworker_config(path: &Path, s: &PanelSettings) -> std::io::Result<()> {
    let cpu_only = s.gpu.slug == "none";
    let (wg, us) = if cpu_only {
        (0, 0)
    } else {
        (s.work_groups, s.unit_size)
    };
    let body = format!(
        r"; Generated by miner-panel.exe
connect = {connect}
supervene = {sv}
nonce_max = 4294967295
notice_wait = 45

{efficiency}
[gpu]
use_opencl = {use_ocl}
cpu_assist = {cpu_assist}
gpu_profile = {profile}
platform_id = {platform_id}
device_ids = {device_id}
opencl_dir = {opencl_dir}
work_groups = {wg}
local_size = 256
unit_size = {us}
debug = 0
",
        connect = s.connect,
        sv = s.cpu.supervene,
        efficiency = efficiency_section(s),
        use_ocl = if cpu_only { "false" } else { "true" },
        cpu_assist = if cpu_only { "false" } else { "true" },
        profile = s.gpu.profile,
        platform_id = s.platform_id,
        device_id = s.device_id,
        opencl_dir = s.opencl_dir,
        wg = wg,
        us = us,
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, body)
}

pub fn write_diaworker_config(path: &Path, s: &PanelSettings) -> std::io::Result<()> {
    let cpu_only = s.gpu.slug == "none";
    let (wg, us) = if cpu_only {
        (0, 0)
    } else {
        (s.work_groups, s.unit_size)
    };
    let body = format!(
        r"; Generated by miner-panel.exe (HACD / diamond mining)
connect = {connect}
supervene = {sv}

{efficiency}
[gpu]
use_opencl = {use_ocl}
cpu_assist = {cpu_assist}
gpu_profile = {profile}
platform_id = {platform_id}
device_ids = {device_id}
opencl_dir = {opencl_dir}
work_groups = {wg}
local_size = 256
unit_size = {us}
debug = 0
",
        connect = s.connect,
        sv = s.cpu.supervene,
        efficiency = efficiency_section(s),
        use_ocl = if cpu_only { "false" } else { "true" },
        cpu_assist = if cpu_only { "false" } else { "true" },
        profile = s.gpu.profile,
        platform_id = s.platform_id,
        device_id = s.device_id,
        opencl_dir = s.opencl_dir,
        wg = wg,
        us = us,
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, body)
}

pub fn write_poworker_benchmark_config(path: &Path, s: &PanelSettings, seconds: u32) -> std::io::Result<()> {
    let mut bench = s.clone_settings();
    bench.benchmark_seconds = seconds;
    bench.benchmark_fine_sweep = seconds >= 60;
    write_poworker_config(path, &bench)
}

impl PanelSettings {
    fn clone_settings(&self) -> PanelSettings {
        PanelSettings {
            cpu: self.cpu.clone(),
            gpu: self.gpu.clone(),
            mode: self.mode,
            power_cost_kwh: self.power_cost_kwh,
            hac_price: self.hac_price,
            platform_id: self.platform_id,
            device_id: self.device_id,
            connect: self.connect.clone(),
            stats_file: self.stats_file.clone(),
            opencl_dir: self.opencl_dir.clone(),
            max_temp_c: self.max_temp_c,
            pause_if_unprofitable: self.pause_if_unprofitable,
            benchmark_seconds: self.benchmark_seconds,
            idle_start_hour: self.idle_start_hour,
            idle_end_hour: self.idle_end_hour,
            benchmark_fine_sweep: self.benchmark_fine_sweep,
            thermal_gpu_index: self.thermal_gpu_index,
            work_groups: self.work_groups,
            unit_size: self.unit_size,
        }
    }
}