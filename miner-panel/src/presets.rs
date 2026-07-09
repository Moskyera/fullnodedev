use app::efficiency::{EfficiencyConf, EfficiencyMode, resolve_gpu_tuning};

#[derive(Clone)]
pub struct CpuPreset {
    pub label: &'static str,
    pub supervene: u32,
}

#[derive(Clone)]
pub struct GpuPreset {
    pub label: &'static str,
    pub slug: &'static str,
    pub profile: &'static str,
    /// Typical board power (W) for kH/J and profit estimates.
    pub watts: f64,
}

pub fn cpu_presets() -> Vec<CpuPreset> {
    vec![
        CpuPreset { label: "Ryzen 5 (5600X, 7600X)", supervene: 4 },
        CpuPreset { label: "Ryzen 7 (5800X, 7700X)", supervene: 6 },
        CpuPreset { label: "Ryzen 9 (5900X, 7900X)", supervene: 8 },
        CpuPreset { label: "Ryzen 9 9950X", supervene: 10 },
        CpuPreset { label: "Threadripper 7960X", supervene: 14 },
        CpuPreset { label: "Threadripper 7970X", supervene: 18 },
        CpuPreset { label: "Threadripper 7980X", supervene: 22 },
        CpuPreset { label: "Intel Core i5 (12400, 13400)", supervene: 4 },
        CpuPreset { label: "Intel Core i7 (12700, 13700)", supervene: 6 },
        CpuPreset { label: "Intel Core i9 (12900, 13900)", supervene: 8 },
        CpuPreset { label: "Intel Core i9 (14900, 24-core)", supervene: 10 },
        CpuPreset { label: "CPU only (no GPU)", supervene: 10 },
    ]
}

pub fn gpu_presets() -> Vec<GpuPreset> {
    vec![
        GpuPreset { label: "RX 6600 / 6600 XT (8GB)", slug: "rx6600", profile: "amd_balanced", watts: 130.0 },
        GpuPreset { label: "RX 7600 (8GB)", slug: "rx7600", profile: "amd_balanced", watts: 165.0 },
        GpuPreset { label: "RX 6700 XT (12GB)", slug: "rx6700xt", profile: "amd_performance", watts: 220.0 },
        GpuPreset { label: "RX 6800 / 6800 XT (16GB)", slug: "rx6800xt", profile: "amd_performance", watts: 260.0 },
        GpuPreset { label: "RX 7900 XT (20GB)", slug: "rx7900xt", profile: "amd_performance", watts: 300.0 },
        GpuPreset { label: "RX 7900 XTX (24GB)", slug: "rx7900xtx", profile: "amd_max", watts: 355.0 },
        GpuPreset { label: "RX 9070 XT (16GB)", slug: "rx9070xt", profile: "amd_performance", watts: 280.0 },
        GpuPreset { label: "GTX 1660 / RTX 3060 (8GB)", slug: "rtx3060", profile: "nvidia_balanced", watts: 170.0 },
        GpuPreset { label: "RTX 3060 Ti / 4060 (8GB)", slug: "rtx4060", profile: "nvidia_balanced", watts: 190.0 },
        GpuPreset { label: "RTX 3070 / 4060 Ti (8-12GB)", slug: "rtx3070", profile: "nvidia_profit", watts: 220.0 },
        GpuPreset { label: "RTX 3080 / 4070 (10-12GB)", slug: "rtx4070", profile: "nvidia_performance", watts: 250.0 },
        GpuPreset { label: "RTX 4080 / 4090 (16GB+)", slug: "rtx4090", profile: "nvidia_max", watts: 320.0 },
        GpuPreset { label: "RTX 5060 (8GB)", slug: "rtx5060", profile: "nvidia_balanced", watts: 150.0 },
        GpuPreset { label: "RTX 5070 / 5070 Ti (12GB)", slug: "rtx5070", profile: "nvidia_performance", watts: 250.0 },
        GpuPreset { label: "RTX 5080 (16GB)", slug: "rtx5080", profile: "nvidia_performance", watts: 320.0 },
        GpuPreset { label: "RTX 5090 (32GB)", slug: "rtx5090", profile: "nvidia_max", watts: 450.0 },
        GpuPreset { label: "No GPU", slug: "none", profile: "", watts: 0.0 },
    ]
}

pub fn gpu_idx_for_profile(gpus: &[GpuPreset], profile: &str) -> Option<usize> {
    gpus.iter().position(|g| g.profile == profile)
}

pub fn tuning_for_profile(profile: &str) -> (u32, u32) {
    let eff = EfficiencyConf {
        mode: EfficiencyMode::Profit,
        power_cost_kwh: 0.15,
        gpu_watts: 0.0,
        cpu_watts_per_thread: 8.0,
        hac_price: 0.0,
        dynamic_supervene: true,
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
        benchmark_fine_sweep: true,
        thermal_gpu_index: 0,
        stats_file: String::new(),
    };
    let mut sec = std::collections::HashMap::new();
    sec.insert("gpu_profile".to_string(), Some(profile.to_string()));
    let t = resolve_gpu_tuning(&sec, &eff);
    (t.workgroups, t.unitsize)
}