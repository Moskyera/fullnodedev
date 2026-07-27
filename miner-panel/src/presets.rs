use app::efficiency::{EfficiencyMode, profile_tuning};
use app::gpu_arch;
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
    /// VRAM in GB: used for safe work_groups caps.
    pub vram_gb: u8,
    /// Typical board power (W) for kH/J and profit estimates.
    pub watts: f64,
}

/// Effective OpenCL tuning written to poworker.config.ini by the panel.
pub type ResolvedTuning = panel_tuning::ResolvedPanelTuning;

pub fn cpu_presets() -> Vec<CpuPreset> {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(8);
    let automatic = (logical / 4).clamp(2, 8);
    vec![
        CpuPreset {
            label: "GPU only (recommended)",
            supervene: 0,
        },
        CpuPreset {
            label: "Automatic CPU assist (safe)",
            supervene: automatic,
        },
        CpuPreset {
            label: "CPU assist: low",
            supervene: 2,
        },
        CpuPreset {
            label: "CPU assist: medium",
            supervene: 4,
        },
        CpuPreset {
            label: "CPU assist: high",
            supervene: 6,
        },
        CpuPreset {
            label: "CPU assist: very high",
            supervene: 8,
        },
        CpuPreset {
            label: "CPU assist: extreme",
            supervene: 12,
        },
    ]
}

pub fn gpu_presets() -> Vec<GpuPreset> {
    vec![
        GpuPreset {
            label: "RX 6600 / 6600 XT (8GB)",
            slug: "rx6600",
            profile: "amd_balanced",
            vram_gb: 8,
            watts: 130.0,
        },
        GpuPreset {
            label: "RX 7600 (8GB)",
            slug: "rx7600",
            profile: "amd_balanced",
            vram_gb: 8,
            watts: 165.0,
        },
        GpuPreset {
            label: "RX 6700 XT (12GB)",
            slug: "rx6700xt",
            profile: "amd_performance",
            vram_gb: 12,
            watts: 220.0,
        },
        GpuPreset {
            label: "RX 6800 / 6800 XT (16GB)",
            slug: "rx6800xt",
            profile: "amd_performance",
            vram_gb: 16,
            watts: 260.0,
        },
        GpuPreset {
            label: "RX 7900 XT (20GB)",
            slug: "rx7900xt",
            profile: "amd_performance",
            vram_gb: 20,
            watts: 300.0,
        },
        GpuPreset {
            label: "RX 7900 XTX (24GB)",
            slug: "rx7900xtx",
            profile: "amd_max",
            vram_gb: 24,
            watts: 355.0,
        },
        GpuPreset {
            label: "RX 9070 XT (16GB)",
            slug: "rx9070xt",
            profile: "amd_balanced",
            vram_gb: 16,
            watts: 280.0,
        },
        GpuPreset {
            label: "GTX 1660 / RTX 3060 (8GB)",
            slug: "rtx3060",
            profile: "nvidia_balanced",
            vram_gb: 8,
            watts: 170.0,
        },
        GpuPreset {
            label: "RTX 3060 Ti / 4060 (8GB)",
            slug: "rtx4060",
            profile: "nvidia_balanced",
            vram_gb: 8,
            watts: 190.0,
        },
        GpuPreset {
            label: "RTX 3070 / 4060 Ti (8-12GB)",
            slug: "rtx3070",
            profile: "nvidia_profit",
            vram_gb: 12,
            watts: 220.0,
        },
        GpuPreset {
            label: "RTX 3080 / 4070 (10-12GB)",
            slug: "rtx4070",
            profile: "nvidia_performance",
            vram_gb: 12,
            watts: 250.0,
        },
        GpuPreset {
            label: "RTX 4080 / 4090 (16GB+)",
            slug: "rtx4090",
            profile: "nvidia_max",
            vram_gb: 24,
            watts: 320.0,
        },
        GpuPreset {
            label: "RTX 5060 (8GB)",
            slug: "rtx5060",
            profile: "nvidia_balanced",
            vram_gb: 8,
            watts: 150.0,
        },
        GpuPreset {
            label: "RTX 5070 / 5070 Ti (12GB)",
            slug: "rtx5070",
            profile: "nvidia_performance",
            vram_gb: 12,
            watts: 250.0,
        },
        GpuPreset {
            label: "RTX 5080 (16GB)",
            slug: "rtx5080",
            profile: "nvidia_performance",
            vram_gb: 16,
            watts: 320.0,
        },
        GpuPreset {
            label: "RTX 5090 (32GB)",
            slug: "rtx5090",
            profile: "nvidia_max",
            vram_gb: 32,
            watts: 450.0,
        },
        GpuPreset {
            label: "Intel Arc A310 / A380 (6GB)",
            slug: "arc_a380",
            profile: "intel_balanced",
            vram_gb: 6,
            watts: 75.0,
        },
        GpuPreset {
            label: "Intel Arc A580 / A750 (8GB)",
            slug: "arc_a750",
            profile: "intel_performance",
            vram_gb: 8,
            watts: 225.0,
        },
        GpuPreset {
            label: "Intel Arc A770 (16GB)",
            slug: "arc_a770",
            profile: "intel_performance",
            vram_gb: 16,
            watts: 225.0,
        },
        GpuPreset {
            label: "No GPU",
            slug: "none",
            profile: "",
            vram_gb: 0,
            watts: 0.0,
        },
    ]
}

pub fn gpu_idx_for_slug(gpus: &[GpuPreset], slug: &str) -> Option<usize> {
    gpus.iter().position(|g| g.slug == slug)
}

pub fn gpu_idx_for_profile(gpus: &[GpuPreset], profile: &str) -> Option<usize> {
    gpus.iter().position(|g| g.profile == profile)
}

/// Match the OpenCL-reported board/architecture to the safest panel preset.
/// AMD drivers often expose only an architecture name (for example `gfx1201`),
/// so VRAM is used as a secondary discriminator.
pub fn gpu_idx_for_opencl(
    gpus: &[GpuPreset],
    device_name: &str,
    device_slug: &str,
    vram_mb: u64,
) -> Option<usize> {
    let name = device_name.to_ascii_lowercase();
    let slug = device_slug.to_ascii_lowercase();
    let preset = if name.contains("9070") || slug == "gfx1201" {
        "rx9070xt"
    } else if name.contains("7900 xtx") || (slug == "gfx1100" && vram_mb >= 22_000) {
        "rx7900xtx"
    } else if name.contains("7900 xt") || slug == "gfx1100" {
        "rx7900xt"
    } else if name.contains("6800") || (slug == "gfx1030" && vram_mb >= 14_000) {
        "rx6800xt"
    } else if name.contains("6700") || slug == "gfx1031" {
        "rx6700xt"
    } else if name.contains("7600") || slug == "gfx1102" {
        "rx7600"
    } else if name.contains("6600") || slug == "gfx1032" {
        "rx6600"
    } else if name.contains("5090") {
        "rtx5090"
    } else if name.contains("5080") {
        "rtx5080"
    } else if name.contains("5070") {
        "rtx5070"
    } else if name.contains("5060") {
        "rtx5060"
    } else if name.contains("4090") || name.contains("4080") {
        "rtx4090"
    } else if name.contains("4070") || name.contains("3080") {
        "rtx4070"
    } else if name.contains("4060") || name.contains("3060 ti") {
        "rtx4060"
    } else if name.contains("3070") {
        "rtx3070"
    } else if name.contains("3060") || name.contains("1660") {
        "rtx3060"
    } else if name.contains("a770") {
        "arc_a770"
    } else if name.contains("a750") || name.contains("a580") {
        "arc_a750"
    } else if name.contains("a380") || name.contains("a310") {
        "arc_a380"
    } else if name.contains("arc") || slug.starts_with("arc") {
        if vram_mb >= 12_000 {
            "arc_a770"
        } else if vram_mb >= 7_000 {
            "arc_a750"
        } else {
            "arc_a380"
        }
    } else if name.contains("radeon") || slug.starts_with("gfx") {
        if vram_mb >= 22_000 {
            "rx7900xtx"
        } else if vram_mb >= 18_000 {
            "rx7900xt"
        } else if vram_mb >= 14_000 {
            "rx6800xt"
        } else if vram_mb >= 10_000 {
            "rx6700xt"
        } else {
            "rx7600"
        }
    } else if name.contains("geforce") || slug.starts_with("rtx") || slug.starts_with("gtx") {
        if vram_mb >= 28_000 {
            "rtx5090"
        } else if vram_mb >= 15_000 {
            "rtx4090"
        } else if vram_mb >= 10_000 {
            "rtx4070"
        } else {
            "rtx3060"
        }
    } else {
        return None;
    };
    gpu_idx_for_slug(gpus, preset)
}

/// True when the preset's profile is an NVIDIA GPU (where the optional CUDA backend applies).
pub fn profile_is_nvidia(profile: &str) -> bool {
    gpu_arch::profile_vendor(profile) == gpu_arch::GpuVendor::Nvidia
}

/// Resolve profile + work_groups + unit_size for a GPU preset and efficiency mode.
pub fn resolve_panel_tuning(gpu: &GpuPreset, mode: EfficiencyMode) -> ResolvedTuning {
    panel_tuning::resolve_panel_tuning(gpu.slug, gpu.profile, gpu.vram_gb, mode)
}

pub fn min_work_groups_for_gpu(slug: &str) -> u32 {
    gpu_arch::panel_min_work_groups(slug)
}

/// Legacy helper: prefer `resolve_panel_tuning`.
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

    #[test]
    fn opencl_gfx1201_auto_selects_rx9070xt() {
        let gpus = gpu_presets();
        let idx = gpu_idx_for_opencl(&gpus, "gfx1201", "gfx1201", 16_304).unwrap();
        assert_eq!(gpus[idx].slug, "rx9070xt");
    }

    #[test]
    fn first_run_gfx1201_replaces_generic_setup_tuning() {
        let gpus = gpu_presets();
        let idx = gpu_idx_for_opencl(&gpus, "gfx1201", "gfx1201", 16_304).unwrap();
        let detected = &gpus[idx];
        let safe = resolve_panel_tuning(detected, EfficiencyMode::Profit);

        let mut profile = "amd_profit".to_string();
        let mut work_groups = 1536;
        let mut unit_size = 96;
        assert_eq!(
            (profile.as_str(), work_groups, unit_size),
            ("amd_profit", 1536, 96)
        );
        profile = safe.profile.to_string();
        work_groups = safe.work_groups;
        unit_size = safe.unit_size;

        assert_eq!(detected.slug, "rx9070xt");
        assert_eq!(
            (profile.as_str(), work_groups, unit_size),
            (safe.profile, 48, 48)
        );
        assert_ne!((work_groups, unit_size), (1536, 96));
    }

    #[test]
    fn opencl_intel_arc_auto_selects_matching_preset() {
        let gpus = gpu_presets();
        let idx =
            gpu_idx_for_opencl(&gpus, "Intel(R) Arc(TM) A770 Graphics", "arca770", 16_384).unwrap();
        assert_eq!(gpus[idx].slug, "arc_a770");
    }

    #[test]
    fn every_gpu_preset_has_safe_tuning_for_every_mode() {
        use app::efficiency::{
            benchmark_candidates_for_device, min_profile_tier_for_mode, profile_tier,
        };
        use app::gpu_arch::{ArchLimits, GpuVendor, profile_vendor};

        for gpu in gpu_presets() {
            if gpu.slug == "none" {
                for mode in [
                    EfficiencyMode::Eco,
                    EfficiencyMode::Profit,
                    EfficiencyMode::Max,
                ] {
                    let tuning = resolve_panel_tuning(&gpu, mode);
                    assert_eq!((tuning.work_groups, tuning.unit_size), (0, 0));
                }
                continue;
            }
            let vendor = profile_vendor(gpu.profile);
            assert_ne!(vendor, GpuVendor::Unknown, "{}", gpu.slug);
            let max_wg = ArchLimits::panel_max_work_groups(gpu.slug, gpu.vram_gb);
            let max_us = ArchLimits::panel_max_unit_size(gpu.slug);
            let min_wg = min_work_groups_for_gpu(gpu.slug);
            let max_tier = ArchLimits::panel_max_tier(gpu.slug);
            for mode in [
                EfficiencyMode::Eco,
                EfficiencyMode::Profit,
                EfficiencyMode::Max,
            ] {
                let tuning = resolve_panel_tuning(&gpu, mode);
                assert_eq!(profile_vendor(tuning.profile), vendor, "{}", gpu.slug);
                assert!(
                    tuning.work_groups >= min_wg && tuning.work_groups <= max_wg,
                    "{}",
                    gpu.slug
                );
                assert!(
                    tuning.unit_size >= 32 && tuning.unit_size <= max_us,
                    "{}",
                    gpu.slug
                );
                assert!(
                    profile_tier(tuning.profile) >= min_profile_tier_for_mode(mode),
                    "{}",
                    gpu.slug
                );
                assert!(profile_tier(tuning.profile) <= max_tier, "{}", gpu.slug);

                let candidates = benchmark_candidates_for_device(
                    vendor,
                    min_profile_tier_for_mode(mode),
                    max_tier,
                    min_wg,
                    max_wg,
                    max_us,
                );
                assert!(!candidates.is_empty(), "{}", gpu.slug);
                assert!(
                    candidates.iter().all(|pick| {
                        pick.workgroups >= min_wg
                            && pick.workgroups <= max_wg
                            && pick.unitsize <= max_us
                            && profile_vendor(&pick.profile) == vendor
                    }),
                    "{}",
                    gpu.slug
                );
            }
        }
    }
}
