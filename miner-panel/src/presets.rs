use app::efficiency::{EfficiencyMode, profile_tuning};
use app::gpu_arch::{self, GpuVendor};
use app::panel_tuning;

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
    /// VRAM in GB — used for safe work_groups caps.
    pub vram_gb: u8,
    /// Typical board power (W) for kH/J and profit estimates.
    pub watts: f64,
}

/// Effective OpenCL tuning written to poworker.config.ini by the panel.
pub type ResolvedTuning = panel_tuning::ResolvedPanelTuning;

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
        GpuPreset { label: "RX 6600 / 6600 XT (8GB)", slug: "rx6600", profile: "amd_balanced", vram_gb: 8, watts: 130.0 },
        GpuPreset { label: "RX 7600 (8GB)", slug: "rx7600", profile: "amd_balanced", vram_gb: 8, watts: 165.0 },
        GpuPreset { label: "RX 6700 XT (12GB)", slug: "rx6700xt", profile: "amd_performance", vram_gb: 12, watts: 220.0 },
        GpuPreset { label: "RX 6800 / 6800 XT (16GB)", slug: "rx6800xt", profile: "amd_performance", vram_gb: 16, watts: 260.0 },
        GpuPreset { label: "RX 7900 XT (20GB)", slug: "rx7900xt", profile: "amd_performance", vram_gb: 20, watts: 300.0 },
        GpuPreset { label: "RX 7900 XTX (24GB)", slug: "rx7900xtx", profile: "amd_max", vram_gb: 24, watts: 355.0 },
        GpuPreset { label: "RX 9070 XT (16GB)", slug: "rx9070xt", profile: "amd_balanced", vram_gb: 16, watts: 280.0 },
        GpuPreset { label: "GTX 1660 / RTX 3060 (8GB)", slug: "rtx3060", profile: "nvidia_balanced", vram_gb: 8, watts: 170.0 },
        GpuPreset { label: "RTX 3060 Ti / 4060 (8GB)", slug: "rtx4060", profile: "nvidia_balanced", vram_gb: 8, watts: 190.0 },
        GpuPreset { label: "RTX 3070 / 4060 Ti (8-12GB)", slug: "rtx3070", profile: "nvidia_profit", vram_gb: 12, watts: 220.0 },
        GpuPreset { label: "RTX 3080 / 4070 (10-12GB)", slug: "rtx4070", profile: "nvidia_performance", vram_gb: 12, watts: 250.0 },
        GpuPreset { label: "RTX 4080 / 4090 (16GB+)", slug: "rtx4090", profile: "nvidia_max", vram_gb: 24, watts: 320.0 },
        GpuPreset { label: "RTX 5060 (8GB)", slug: "rtx5060", profile: "nvidia_balanced", vram_gb: 8, watts: 150.0 },
        GpuPreset { label: "RTX 5070 / 5070 Ti (12GB)", slug: "rtx5070", profile: "nvidia_performance", vram_gb: 12, watts: 250.0 },
        GpuPreset { label: "RTX 5080 (16GB)", slug: "rtx5080", profile: "nvidia_performance", vram_gb: 16, watts: 320.0 },
        GpuPreset { label: "RTX 5090 (32GB)", slug: "rtx5090", profile: "nvidia_max", vram_gb: 32, watts: 450.0 },
        GpuPreset { label: "No GPU", slug: "none", profile: "", vram_gb: 0, watts: 0.0 },
    ]
}

pub fn gpu_idx_for_slug(gpus: &[GpuPreset], slug: &str) -> Option<usize> {
    gpus.iter().position(|g| g.slug == slug)
}

pub fn gpu_idx_for_profile(gpus: &[GpuPreset], profile: &str) -> Option<usize> {
    gpus.iter().position(|g| g.profile == profile)
}

pub fn is_rdna4_experimental(slug: &str) -> bool {
    gpu_arch::ArchLimits::for_panel_slug(slug).is_experimental()
}

/// Resolve profile + work_groups + unit_size for a GPU preset and efficiency mode.
pub fn resolve_panel_tuning(gpu: &GpuPreset, mode: EfficiencyMode) -> ResolvedTuning {
    panel_tuning::resolve_panel_tuning(gpu.slug, gpu.profile, gpu.vram_gb, mode)
}

pub fn min_work_groups_for_gpu(slug: &str) -> u32 {
    gpu_arch::panel_min_work_groups(slug)
}

/// Legacy helper — prefer `resolve_panel_tuning`.
pub fn tuning_for_profile(profile: &str) -> (u32, u32) {
    profile_tuning(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(slug: &str) -> GpuPreset {
        gpu_presets()
            .into_iter()
            .find(|g| g.slug == slug)
            .unwrap_or_else(|| panic!("unknown slug {slug}"))
    }

    #[test]
    fn rx9070xt_max_wg_capped() {
        let t = resolve_panel_tuning(&gpu("rx9070xt"), EfficiencyMode::Max);
        assert_eq!(t.work_groups, 64);
        assert_eq!(t.unit_size, 64);
    }

    #[test]
    fn rx7900xtx_max_wg_high() {
        let t = resolve_panel_tuning(&gpu("rx7900xtx"), EfficiencyMode::Max);
        assert!(t.work_groups >= 1024);
    }
}