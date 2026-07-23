use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use app::efficiency::{EfficiencyMode, min_profile_tier_for_mode, profile_tier};
use app::gpu_arch::{ArchLimits, GpuVendor, normalize_profile, profile_vendor};

use crate::currency::Currency;

use crate::presets::{
    CpuPreset, GpuPreset, gpu_idx_for_profile, gpu_idx_for_slug, min_work_groups_for_gpu,
    resolve_panel_tuning, tuning_for_profile,
};

pub struct PanelSettings {
    pub cpu: CpuPreset,
    pub gpu: GpuPreset,
    /// Effective gpu_profile written to ini (may differ from gpu.profile when mode shifts tier).
    pub gpu_profile: String,
    pub mode: EfficiencyMode,
    pub power_cost_kwh: f64,
    pub hac_price: f64,
    pub platform_id: u32,
    pub device_id: u32,
    /// Use the CUDA backend (NVIDIA) instead of OpenCL. Requires a miner built with
    /// `--features cuda`; only honored for NVIDIA GPUs (write_poworker_config gates it).
    pub use_cuda: bool,
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
    /// poworker: max nonce searched per batch (default u32::MAX).
    pub nonce_max: u32,
    /// poworker: seconds to wait for a new-block notice (default 45).
    pub notice_wait: u64,
    /// Payout address announced to a pool (`pool_worker`). Set only in Pool
    /// mode; empty for solo so the worker's requests stay identical to a plain
    /// fullnode's.
    pub pool_worker: String,
}

const BENCHMARK_BACKUP_PRESENT: &str = "HACASH_MINER_PANEL_AUTOTUNE_BACKUP_V1:PRESENT\n";
const BENCHMARK_BACKUP_ABSENT: &str = "HACASH_MINER_PANEL_AUTOTUNE_BACKUP_V1:ABSENT\n";

/// Durable marker used to recover the exact pre-benchmark config after a
/// panel/worker crash. The sidecar remains until Auto Tune commits or rolls
/// back successfully.
#[derive(Debug)]
pub struct BenchmarkConfigBackup {
    sidecar_path: PathBuf,
}

fn benchmark_backup_path(config_path: &Path) -> PathBuf {
    let mut name = config_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("poworker.config.ini"))
        .to_os_string();
    name.push(".autotune-backup");
    config_path.with_file_name(name)
}

pub fn create_benchmark_backup(config_path: &Path) -> io::Result<BenchmarkConfigBackup> {
    let sidecar_path = benchmark_backup_path(config_path);
    if sidecar_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "an interrupted Auto Tune backup already exists: {}",
                sidecar_path.display()
            ),
        ));
    }
    let body = match std::fs::read_to_string(config_path) {
        Ok(content) => format!("{BENCHMARK_BACKUP_PRESENT}{content}"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            BENCHMARK_BACKUP_ABSENT.to_string()
        }
        Err(error) => return Err(error),
    };
    crate::hacash_config::atomic_write_private(&sidecar_path, &body)?;
    Ok(BenchmarkConfigBackup { sidecar_path })
}

pub fn restore_benchmark_backup(
    config_path: &Path,
    backup: &BenchmarkConfigBackup,
) -> io::Result<()> {
    let raw = std::fs::read_to_string(&backup.sidecar_path)?;
    if let Some(content) = raw.strip_prefix(BENCHMARK_BACKUP_PRESENT) {
        crate::hacash_config::atomic_write_private(config_path, content)?;
    } else if raw == BENCHMARK_BACKUP_ABSENT {
        match std::fs::remove_file(config_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Auto Tune backup marker",
        ));
    }
    std::fs::remove_file(&backup.sidecar_path)
}

pub fn commit_benchmark_backup(backup: &BenchmarkConfigBackup) -> io::Result<()> {
    std::fs::remove_file(&backup.sidecar_path)
}

pub fn recover_interrupted_benchmark(config_path: &Path) -> io::Result<bool> {
    let sidecar_path = benchmark_backup_path(config_path);
    if !sidecar_path.exists() {
        return Ok(false);
    }
    let backup = BenchmarkConfigBackup { sidecar_path };
    restore_benchmark_backup(config_path, &backup)?;
    Ok(true)
}

/// Return the durable rollback marker after a failed startup recovery so the
/// UI can stay locked and offer an explicit retry instead of hiding the error.
pub fn interrupted_benchmark_backup(config_path: &Path) -> Option<BenchmarkConfigBackup> {
    let sidecar_path = benchmark_backup_path(config_path);
    sidecar_path
        .exists()
        .then_some(BenchmarkConfigBackup { sidecar_path })
}

#[derive(Default)]
pub struct LoadedPanelIni {
    pub supervene: Option<u32>,
    pub gpu_slug: Option<String>,
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
    pub benchmark_seconds: Option<u32>,
    pub use_cuda: Option<bool>,
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
            out.insert(
                k.trim().to_lowercase(),
                v.split(';').next().unwrap_or(v).trim().to_string(),
            );
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
        gpu_slug: gpu.get("gpu_slug").cloned(),
        gpu_profile: gpu.get("gpu_profile").cloned(),
        work_groups: gpu.get("work_groups").and_then(|v| parse_u32(v)),
        unit_size: gpu.get("unit_size").and_then(|v| parse_u32(v)),
        platform_id: gpu.get("platform_id").and_then(|v| parse_u32(v)),
        device_id: gpu
            .get("device_ids")
            .and_then(|v| v.split(',').next())
            .and_then(|v| parse_u32(v)),
        connect: root_value(&content, "connect"),
        mode: eff.get("mode").map(|s| EfficiencyMode::from_str(s)),
        power_cost_kwh: eff.get("power_cost_kwh").and_then(|v| parse_f64(v)),
        hac_price: eff.get("hac_price").and_then(|v| parse_f64(v)),
        max_temp_c: eff.get("max_temp_c").and_then(|v| parse_u32(v)),
        pause_if_unprofitable: eff
            .get("pause_if_unprofitable")
            .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes")),
        benchmark_seconds: eff.get("benchmark_seconds").and_then(|v| parse_u32(v)),
        use_cuda: gpu
            .get("use_cuda")
            .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes")),
    }
}

/// Load user preferences from ini. Does not load work_groups / unit_size: those come from
/// `resolve_panel_tuning` unless benchmark results are applied separately.
pub fn apply_loaded_ini(
    loaded: &LoadedPanelIni,
    cpus: &[CpuPreset],
    gpus: &[GpuPreset],
    cpu_idx: &mut usize,
    gpu_idx: &mut usize,
    mode_idx: &mut usize,
    platform_id: &mut u32,
    device_id: &mut u32,
    connect: &mut String,
    power_cost: &mut f32,
    hac_price: &mut f32,
    max_temp_c: &mut u32,
    pause_unprofitable: &mut bool,
    use_cuda: &mut bool,
    currency: Currency,
) {
    if let Some(sv) = loaded.supervene {
        if let Some(i) = cpus.iter().position(|c| c.supervene == sv) {
            *cpu_idx = i;
        }
    }
    if let Some(ref slug) = loaded.gpu_slug {
        if let Some(i) = gpu_idx_for_slug(gpus, slug) {
            *gpu_idx = i;
        }
    } else if let Some(ref profile) = loaded.gpu_profile {
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
        *power_cost = Currency::convert(c, Currency::Eur, currency) as f32;
    }
    if let Some(p) = loaded.hac_price {
        *hac_price = Currency::convert(p, Currency::Eur, Currency::Usd) as f32;
    }
    if let Some(t) = loaded.max_temp_c {
        *max_temp_c = t;
    }
    if let Some(p) = loaded.pause_if_unprofitable {
        *pause_unprofitable = p;
    }
    if let Some(c) = loaded.use_cuda {
        *use_cuda = c;
    }
}

fn safe_profile_for_gpu(gpu: &GpuPreset, requested: &str, mode: EfficiencyMode) -> String {
    let default = resolve_panel_tuning(gpu, mode);
    let vendor = profile_vendor(gpu.profile);
    let candidate = if requested.trim().is_empty() {
        default.profile.to_string()
    } else {
        normalize_profile(requested, vendor)
    };
    let tier = profile_tier(&candidate);
    if profile_vendor(&candidate) != vendor
        || tier < min_profile_tier_for_mode(mode)
        || tier > ArchLimits::panel_max_tier(gpu.slug)
    {
        default.profile.to_string()
    } else {
        candidate
    }
}

/// Load the exact successful autotune point and clamp only to hard device
/// limits. Mode defaults must not overwrite a measured result.
pub fn apply_benchmark_ini(
    loaded: &LoadedPanelIni,
    gpus: &[GpuPreset],
    gpu_idx: &mut usize,
    work_groups: &mut u32,
    unit_size: &mut u32,
    gpu_profile: &mut String,
    mode: EfficiencyMode,
) {
    if let Some(ref slug) = loaded.gpu_slug {
        if let Some(i) = gpu_idx_for_slug(gpus, slug) {
            *gpu_idx = i;
        }
    } else if let Some(ref profile) = loaded.gpu_profile {
        if let Some(i) = gpu_idx_for_profile(gpus, profile) {
            *gpu_idx = i;
        }
    }
    let Some(gpu) = gpus.get(*gpu_idx) else {
        return;
    };
    if gpu.slug == "none" {
        *work_groups = 0;
        *unit_size = 0;
        gpu_profile.clear();
        return;
    }

    let requested_profile = loaded
        .gpu_profile
        .as_deref()
        .unwrap_or(gpu_profile.as_str());
    *gpu_profile = safe_profile_for_gpu(gpu, requested_profile, mode);
    let (default_wg, default_us) = tuning_for_profile(gpu_profile);
    let min_wg = min_work_groups_for_gpu(&gpu.slug);
    let max_wg = ArchLimits::panel_max_work_groups(&gpu.slug, gpu.vram_gb);
    let max_us = ArchLimits::panel_max_unit_size(&gpu.slug);
    *work_groups = loaded
        .work_groups
        .unwrap_or(default_wg)
        .clamp(min_wg, max_wg);
    *unit_size = loaded.unit_size.unwrap_or(default_us).clamp(32, max_us);
}

/// Resolve UI/default/autotune tuning. Explicit benchmark values are
/// preserved and only hard architecture/VRAM caps are applied.
fn resolve_ini_tuning(s: &PanelSettings) -> (u32, u32, String) {
    if s.gpu.slug == "none" {
        return (0, 0, String::new());
    }
    let default = resolve_panel_tuning(&s.gpu, s.mode);
    let profile = safe_profile_for_gpu(&s.gpu, &s.gpu_profile, s.mode);
    let min_wg = min_work_groups_for_gpu(&s.gpu.slug);
    let max_wg = ArchLimits::panel_max_work_groups(&s.gpu.slug, s.gpu.vram_gb);
    let max_us = ArchLimits::panel_max_unit_size(&s.gpu.slug);
    let wg = if s.work_groups > 0 {
        s.work_groups.clamp(min_wg, max_wg)
    } else {
        default.work_groups
    };
    let us = if s.unit_size > 0 {
        s.unit_size.clamp(32, max_us)
    } else {
        default.unit_size
    };
    (wg, us, profile)
}

fn efficiency_section(
    s: &PanelSettings,
    throttle_work_groups: u32,
    gpu_watts: f64,
    max_temp_c: u32,
    supervene: u32,
) -> String {
    let mode = s.mode.label();
    format!(
        r"[efficiency]
mode = {mode}
power_cost_kwh = {cost}
gpu_watts = {gpu_watts}
cpu_watts_per_thread = 8
hac_price = {hac_price}
dynamic_supervene = {dynamic_supervene}
supervene_min = {sv_min}
supervene_max = {sv}
oom_fallback = true
max_temp_c = {max_temp}
throttle_work_groups = {throttle_work_groups}
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
        gpu_watts = gpu_watts,
        hac_price = s.hac_price,
        sv = supervene,
        dynamic_supervene = supervene > 0,
        sv_min = if supervene > 0 { 1 } else { 0 },
        max_temp = max_temp_c,
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
    // CUDA is only a valid backend for NVIDIA GPUs; gate here so a stale checkbox never
    // writes use_cuda=true for an AMD/Intel selection.
    let cuda_on = s.use_cuda && !cpu_only && profile_vendor(s.gpu.profile) == GpuVendor::Nvidia;
    let cpu_assist = !cpu_only && s.cpu.supervene > 0;
    let (wg, us, profile) = resolve_ini_tuning(s);
    let body = format!(
        r"; Generated by miner-panel: do not edit by hand; use the panel UI.
connect = {connect}
supervene = {sv}
nonce_max = {nonce_max}
notice_wait = {notice_wait}
pool_worker = {pool_worker}

{efficiency}
[gpu]
use_opencl = {use_ocl}
use_cuda = {use_cuda}
cpu_assist = {cpu_assist}
gpu_slug = {gpu_slug}
gpu_profile = {profile}
platform_id = {platform_id}
device_ids = {device_id}
cuda_device = {device_id}
opencl_dir = {opencl_dir}
work_groups = {wg}
local_size = 256
unit_size = {us}
debug = 0
",
        connect = s.connect,
        sv = s.cpu.supervene,
        nonce_max = s.nonce_max,
        notice_wait = s.notice_wait,
        pool_worker = s.pool_worker,
        // Thermal cap must be below full load; half of WG (min 1) actually reduces heat.
        efficiency = efficiency_section(
            s,
            (wg / 2).max(1),
            s.gpu.watts,
            s.max_temp_c,
            s.cpu.supervene,
        ),
        use_ocl = if cpu_only || cuda_on { "false" } else { "true" },
        use_cuda = if cuda_on { "true" } else { "false" },
        cpu_assist = if cpu_assist { "true" } else { "false" },
        gpu_slug = s.gpu.slug,
        profile = profile,
        platform_id = s.platform_id,
        device_id = s.device_id,
        opencl_dir = s.opencl_dir,
        wg = wg,
        us = us,
    );
    crate::hacash_config::atomic_write_private(path, &body)
}

pub fn write_diaworker_config(path: &Path, s: &PanelSettings) -> std::io::Result<()> {
    // HACD mining is officially CPU/full-node only. Keep this config strict so
    // selecting a GPU for HAC can never leak an experimental GPU path into HACD.
    let supervene = s.cpu.supervene.max(1);
    let body = format!(
        r"; Generated by miner-panel (HACD / diamond mining): CPU/full-node only.
connect = {connect}
supervene = {sv}

{efficiency}
[gpu]
use_opencl = false
use_cuda = false
cpu_assist = false
gpu_slug = none
gpu_profile =
platform_id = 0
device_ids = 0
opencl_dir =
work_groups = 0
local_size = 256
unit_size = 0
debug = 0
",
        connect = s.connect,
        sv = supervene,
        efficiency = efficiency_section(s, 1, 0.0, 0, supervene),
    );
    crate::hacash_config::atomic_write_private(path, &body)
}

pub fn write_poworker_benchmark_config(
    path: &Path,
    s: &PanelSettings,
    seconds: u32,
) -> std::io::Result<()> {
    let mut bench = s.clone_settings();
    bench.benchmark_seconds = seconds;
    bench.benchmark_fine_sweep = seconds >= 60;
    write_poworker_config(path, &bench)?;

    // Allocate for the full safe range of this preset. The worker will record
    // only the exact point that actually ran after runtime CU/VRAM clamping.
    let max_wg = ArchLimits::panel_max_work_groups(&s.gpu.slug, s.gpu.vram_gb);
    let max_us = ArchLimits::panel_max_unit_size(&s.gpu.slug);
    let raw = std::fs::read_to_string(path)?;
    let mut in_gpu = false;
    let mut out = String::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_gpu = trimmed.eq_ignore_ascii_case("[gpu]");
        }
        if in_gpu && trimmed.starts_with("work_groups") && trimmed.contains('=') {
            out.push_str(&format!("work_groups = {max_wg}"));
        } else if in_gpu && trimmed.starts_with("unit_size") && trimmed.contains('=') {
            out.push_str(&format!("unit_size = {max_us}"));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    crate::hacash_config::atomic_write_private(path, &out)
}

#[cfg(test)]
mod write_tuning_tests {
    use super::*;
    use crate::presets::{GpuPreset, cpu_presets, gpu_presets};
    use app::efficiency::EfficiencyMode;

    fn panel_with_wg(gpu: &GpuPreset, wg: u32, us: u32) -> PanelSettings {
        PanelSettings {
            cpu: cpu_presets()[0].clone(),
            gpu: gpu.clone(),
            gpu_profile: gpu.profile.to_string(),
            mode: EfficiencyMode::Max,
            power_cost_kwh: 0.15,
            hac_price: 0.01,
            platform_id: 0,
            device_id: 0,
            use_cuda: false,
            connect: "127.0.0.1:8080".into(),
            stats_file: String::new(),
            opencl_dir: String::new(),
            max_temp_c: 85,
            pause_if_unprofitable: false,
            benchmark_seconds: 0,
            idle_start_hour: 255,
            idle_end_hour: 255,
            benchmark_fine_sweep: false,
            thermal_gpu_index: 0,
            work_groups: wg,
            unit_size: us,
            nonce_max: u32::MAX,
            notice_wait: 45,
            pool_worker: String::new(),
        }
    }

    #[test]
    fn resolve_ini_tuning_caps_benchmark_wg_for_rx9070xt() {
        let gpu = gpu_presets()
            .into_iter()
            .find(|g| g.slug == "rx9070xt")
            .unwrap();
        let s = panel_with_wg(&gpu, 2048, 128);
        let (wg, us, _) = resolve_ini_tuning(&s);
        assert_eq!(wg, 64);
        assert_eq!(us, 64);
    }

    #[test]
    fn resolve_ini_tuning_keeps_benchmark_within_cap() {
        let gpu = gpu_presets()
            .into_iter()
            .find(|g| g.slug == "rx9070xt")
            .unwrap();
        let s = panel_with_wg(&gpu, 128, 64);
        let (wg, us, _) = resolve_ini_tuning(&s);
        assert_eq!(wg, 64);
        assert_eq!(us, 64);
    }

    #[test]
    fn measured_point_is_not_clamped_back_to_mode_default() {
        let gpu = gpu_presets()
            .into_iter()
            .find(|g| g.slug == "rx7900xtx")
            .unwrap();
        let mut s = panel_with_wg(&gpu, 4096, 128);
        s.mode = EfficiencyMode::Eco;
        s.gpu_profile = "amd_max".into();
        let (wg, us, profile) = resolve_ini_tuning(&s);
        assert_eq!((wg, us), (4096, 128));
        assert_eq!(profile, "amd_max");
    }

    #[test]
    fn benchmark_allocates_full_safe_range_for_every_gpu_preset() {
        for gpu in gpu_presets().into_iter().filter(|g| g.slug != "none") {
            let s = panel_with_wg(&gpu, 1, 32);
            let path = std::env::temp_dir().join(format!(
                "hacash-panel-autotune-{}-{}.ini",
                gpu.slug,
                std::process::id()
            ));
            write_poworker_benchmark_config(&path, &s, 90).unwrap();
            let loaded = load_panel_ini(&path);
            let _ = std::fs::remove_file(path);
            assert_eq!(
                loaded.work_groups,
                Some(ArchLimits::panel_max_work_groups(&gpu.slug, gpu.vram_gb)),
                "{}",
                gpu.slug
            );
            assert_eq!(
                loaded.unit_size,
                Some(ArchLimits::panel_max_unit_size(&gpu.slug)),
                "{}",
                gpu.slug
            );
            assert_eq!(loaded.benchmark_seconds, Some(90), "{}", gpu.slug);
        }
    }

    #[test]
    fn gpu_only_config_disables_cpu_assist_and_cuda() {
        let gpu = gpu_presets()
            .into_iter()
            .find(|g| g.slug == "rx9070xt")
            .unwrap();
        let s = panel_with_wg(&gpu, 64, 64);
        let path =
            std::env::temp_dir().join(format!("hacash-panel-config-{}.ini", std::process::id()));
        write_poworker_config(&path, &s).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(path);
        assert!(raw.contains("use_opencl = true"));
        assert!(raw.contains("use_cuda = false"));
        assert!(raw.contains("cpu_assist = false"));
        assert!(raw.contains("supervene = 0"));
        assert!(
            raw.contains("throttle_work_groups = 32"),
            "thermal throttle must be half of work_groups=64, got:\n{raw}"
        );
    }

    #[test]
    fn cuda_enabled_for_nvidia_writes_cuda_backend() {
        let gpu = gpu_presets()
            .into_iter()
            .find(|g| g.slug == "rtx4090")
            .unwrap();
        let mut s = panel_with_wg(&gpu, 64, 64);
        s.use_cuda = true;
        s.device_id = 2;
        let path =
            std::env::temp_dir().join(format!("hacash-panel-cuda-{}.ini", std::process::id()));
        write_poworker_config(&path, &s).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        // CUDA selected on an NVIDIA GPU: OpenCL off, CUDA on, cuda_device carried through.
        assert!(raw.contains("use_cuda = true"), "{raw}");
        assert!(raw.contains("use_opencl = false"), "{raw}");
        assert!(raw.contains("cuda_device = 2"), "{raw}");
        // The written config round-trips back through the loader.
        let tmp =
            std::env::temp_dir().join(format!("hacash-panel-cuda2-{}.ini", std::process::id()));
        std::fs::write(&tmp, &raw).unwrap();
        let loaded = load_panel_ini(&tmp);
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(loaded.use_cuda, Some(true));
    }

    #[test]
    fn cuda_flag_ignored_for_non_nvidia_gpu() {
        // A stale use_cuda=true must never enable CUDA for a non-NVIDIA GPU.
        let gpu = gpu_presets()
            .into_iter()
            .find(|g| g.slug == "rx9070xt")
            .unwrap();
        let mut s = panel_with_wg(&gpu, 64, 64);
        s.use_cuda = true;
        let path =
            std::env::temp_dir().join(format!("hacash-panel-amdcuda-{}.ini", std::process::id()));
        write_poworker_config(&path, &s).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(raw.contains("use_cuda = false"), "{raw}");
        assert!(raw.contains("use_opencl = true"), "{raw}");
    }

    #[test]
    fn pool_mode_announces_the_payout_address_and_solo_stays_empty() {
        let gpu = gpu_presets()
            .into_iter()
            .find(|g| g.slug == "rx9070xt")
            .unwrap();
        let mut s = panel_with_wg(&gpu, 64, 64);
        let path =
            std::env::temp_dir().join(format!("hacash-panel-poolw-{}.ini", std::process::id()));

        // Pool mode: the address the user already typed is announced to the pool
        // so it can credit and pay this miner automatically.
        s.pool_worker = "1NVYv5jmr9JRF3usPZJQmJFJhbQhrPESTP".to_string();
        write_poworker_config(&path, &s).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("pool_worker = 1NVYv5jmr9JRF3usPZJQmJFJhbQhrPESTP"));

        // Solo: left empty, so the worker's URLs stay identical to a plain node.
        s.pool_worker = String::new();
        write_poworker_config(&path, &s).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(path);
        assert!(raw.contains("pool_worker ="));
        assert!(!raw.contains("pool_worker = 1"));
    }

    #[test]
    fn hacd_config_is_strictly_cpu_only() {
        let gpu = gpu_presets()
            .into_iter()
            .find(|g| g.slug == "rx9070xt")
            .unwrap();
        let s = panel_with_wg(&gpu, 64, 64);
        let path =
            std::env::temp_dir().join(format!("hacash-panel-hacd-{}.ini", std::process::id()));
        write_diaworker_config(&path, &s).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(path);
        assert!(raw.contains("supervene = 1"));
        assert!(raw.contains("gpu_watts = 0"));
        assert!(raw.contains("max_temp_c = 0"));
        assert!(raw.contains("use_opencl = false"));
        assert!(raw.contains("use_cuda = false"));
        assert!(raw.contains("cpu_assist = false"));
        assert!(raw.contains("gpu_slug = none"));
        assert!(raw.contains("work_groups = 0"));
        assert!(!raw.contains("use_opencl = true"));
    }

    #[test]
    fn benchmark_completion_marker_is_loaded() {
        let path =
            std::env::temp_dir().join(format!("hacash-panel-benchmark-{}.ini", std::process::id()));
        std::fs::write(
            &path,
            "[efficiency]
benchmark_seconds = 0
[gpu]
work_groups = 64
",
        )
        .unwrap();
        let loaded = load_panel_ini(&path);
        let _ = std::fs::remove_file(path);
        assert_eq!(loaded.benchmark_seconds, Some(0));
        assert_eq!(loaded.work_groups, Some(64));
    }

    #[test]
    fn interrupted_benchmark_restores_exact_config() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hacash-panel-backup-{}-{unique}.ini",
            std::process::id()
        ));
        let original = "; user spacing is preserved\r\nconnect = 127.0.0.1:8081\r\n";
        std::fs::write(&path, original).unwrap();
        let _backup = create_benchmark_backup(&path).unwrap();
        std::fs::write(&path, "benchmark_seconds = 90\n").unwrap();

        assert!(recover_interrupted_benchmark(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!recover_interrupted_benchmark(&path).unwrap());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn interrupted_benchmark_restores_missing_config_state() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hacash-panel-absent-backup-{}-{unique}.ini",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let _backup = create_benchmark_backup(&path).unwrap();
        std::fs::write(&path, "benchmark_seconds = 90\n").unwrap();

        assert!(recover_interrupted_benchmark(&path).unwrap());
        assert!(!path.exists());
    }
}

impl PanelSettings {
    fn clone_settings(&self) -> PanelSettings {
        PanelSettings {
            cpu: self.cpu.clone(),
            gpu: self.gpu.clone(),
            gpu_profile: self.gpu_profile.clone(),
            mode: self.mode,
            power_cost_kwh: self.power_cost_kwh,
            hac_price: self.hac_price,
            platform_id: self.platform_id,
            device_id: self.device_id,
            use_cuda: self.use_cuda,
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
            nonce_max: self.nonce_max,
            notice_wait: self.notice_wait,
            pool_worker: self.pool_worker.clone(),
        }
    }
}
