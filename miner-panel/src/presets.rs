use app::cpu_threads::{cpu_assist_threads_for, hacd_threads_for, logical_cpus};
use app::efficiency::{EfficiencyMode, profile_tuning};
use app::gpu_arch;
use app::panel_tuning;

#[derive(Clone)]
pub struct CpuPreset {
    /// What the operator reads in the picker, including the thread count. This
    /// is a `String` rather than a literal because every entry below is now
    /// derived from the core count of the machine the panel is running on, so
    /// there is no literal to write.
    pub label: String,
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

/// The CPU thread choices this panel offers, built from the machine's own
/// logical CPU count.
///
/// This list used to be seven fixed numbers ending at 12, with an "Automatic"
/// entry of `(logical / 4).clamp(2, 8)`. On a 32-thread CPU that made 8 the
/// automatic answer and 12 the largest thing an operator could ask for, against
/// a measured optimum of 30. There was no way through this GUI to express a
/// modern CPU at all, so a GUI operator was capped at roughly a third of their
/// machine no matter what they clicked.
///
/// Now every entry is a fraction of the real machine, so the list is right on a
/// 4-thread laptop and on a 128-thread Threadripper without anybody editing it
/// again. Duplicates collapse (on small CPUs several fractions land on the same
/// number), the list stays sorted, and "GPU only" stays first because it is the
/// recommended answer for a GPU rig and index 0 is what the rest of the panel
/// falls back to.
pub fn cpu_presets() -> Vec<CpuPreset> {
    cpu_presets_for(logical_cpus())
}

/// `cpu_presets` for an arbitrary machine size, so the ladder is testable
/// without owning the CPU it describes.
pub fn cpu_presets_for(logical: u32) -> Vec<CpuPreset> {
    let logical = logical.max(1);
    let all_but_reserve = hacd_threads_for(logical);
    let assist = cpu_assist_threads_for(logical);

    // (label, threads). Ordered by thread count; the two "Automatic" entries are
    // named for the question they answer, because the panel writes the same
    // number into a GPU rig's CPU assist and into the HACD miner that owns the
    // whole CPU, and those are not the same question.
    let mut rungs: Vec<(&str, u32)> = vec![
        ("Automatic: CPU assist beside a GPU", assist),
        ("Automatic: all cores, best for HACD", all_but_reserve),
        ("An eighth of the CPU", (logical / 8).max(1)),
        ("A quarter of the CPU", (logical / 4).max(1)),
        ("Half the CPU", (logical / 2).max(1)),
        // Divide first: `logical * 3` would overflow a hypothetical huge count.
        ("Three quarters of the CPU", (logical / 4 * 3).max(1)),
        ("Every thread, leaves nothing for the node", logical),
    ];
    rungs.sort_by_key(|(_, threads)| *threads);

    let mut presets = vec![CpuPreset {
        label: "GPU only (recommended)".to_string(),
        supervene: 0,
    }];
    for (label, threads) in rungs {
        if presets.iter().any(|p| p.supervene == threads) {
            continue;
        }
        presets.push(CpuPreset {
            label: format!("{label}: {threads} threads"),
            supervene: threads,
        });
    }
    presets
}

/// The index of the entry closest to `supervene`, for restoring a saved config.
///
/// Nearest, not exact. The ladder is derived from the machine, so a config
/// written on one CPU and opened on another (a copied install, a VPS image, a
/// new build) will usually hold a number this ladder does not contain. Exact
/// matching sent every one of those to index 0, which is "GPU only": an operator
/// who had asked for 20 threads reopened the panel and silently had none.
pub fn cpu_idx_for_supervene(cpus: &[CpuPreset], supervene: u32) -> Option<usize> {
    cpus.iter()
        .enumerate()
        .min_by_key(|(_, c)| c.supervene.abs_diff(supervene))
        .map(|(i, _)| i)
}

/// The rung to move a HACD operator to when the saved selection is "GPU only".
///
/// HACD is CPU-only, so "GPU only" is not a thing it can do and the panel has
/// always substituted something. It used to substitute the first entry with any
/// threads at all, which on the old list was 2 and on the new one is smaller
/// still. The substitute is now the automatic count: the machine.
pub fn hacd_default_idx(cpus: &[CpuPreset]) -> Option<usize> {
    cpu_idx_for_supervene(cpus, app::cpu_threads::hacd_threads()).filter(|i| cpus[*i].supervene > 0)
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

    /// Max on this card is the measured optimum, not a conservative guess.
    ///
    /// 64 x 256 x 192 measured 28.80 MH/s at repeat 16 against 19.13 for the
    /// 48 x 256 x 48 this used to resolve to, on a 0.5% noise floor, and was
    /// proven byte identical to it against the CPU oracle first. The work-group
    /// count is unchanged and was never the limit; the unit_size ceiling was.
    #[test]
    fn rx9070xt_max_is_the_measured_optimum() {
        let t = resolve_panel_tuning(&gpu("rx9070xt"), EfficiencyMode::Max);
        assert_eq!((t.work_groups, t.unit_size), (64, 192));
    }

    /// The 9950X in the picker. Before this, the largest entry a GUI operator
    /// could choose on this CPU was 12 and "Automatic" gave 8, against a
    /// measured optimum of 30: the GUI could not express the machine at all.
    #[test]
    fn a_thirty_two_thread_cpu_can_finally_be_expressed_in_the_gui() {
        let presets = cpu_presets_for(32);
        let counts: Vec<u32> = presets.iter().map(|p| p.supervene).collect();
        assert_eq!(counts, vec![0, 4, 8, 16, 24, 30, 32]);
        // The two numbers that used to be the whole story.
        assert!(counts.iter().max().copied().unwrap() > 12);
        assert!(counts.contains(&30), "all cores but the host reserve");
    }

    /// Every entry names its own thread count, because "high" and "extreme" are
    /// not quantities and the operator is choosing a quantity.
    #[test]
    fn every_label_states_the_number_of_threads_it_means() {
        for logical in [1u32, 2, 4, 8, 12, 16, 32, 64, 128] {
            for preset in cpu_presets_for(logical) {
                if preset.supervene == 0 {
                    continue;
                }
                assert!(
                    preset.label.contains(&preset.supervene.to_string()),
                    "logical={logical}: {:?} does not say {}",
                    preset.label,
                    preset.supervene
                );
            }
        }
    }

    /// The ladder must be a ladder on any machine: sorted, no repeats, nothing
    /// bigger than the CPU, and "GPU only" first because index 0 is what the
    /// rest of the panel falls back to.
    #[test]
    fn the_ladder_is_well_formed_on_every_machine_size() {
        for logical in 1..=256u32 {
            let presets = cpu_presets_for(logical);
            let counts: Vec<u32> = presets.iter().map(|p| p.supervene).collect();
            assert_eq!(counts[0], 0, "logical={logical}");
            assert!(counts.len() >= 2, "logical={logical}: nothing to choose");
            let mut sorted = counts.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(counts, sorted, "logical={logical}");
            assert_eq!(
                *counts.last().unwrap(),
                logical,
                "logical={logical}: the top rung is the whole CPU"
            );
            assert!(
                counts.contains(&app::cpu_threads::hacd_threads_for(logical)),
                "logical={logical}: no entry for the HACD default"
            );
            assert!(
                counts.contains(&app::cpu_threads::cpu_assist_threads_for(logical)),
                "logical={logical}: no entry for the CPU assist default"
            );
        }
    }

    /// A config written on one machine and opened on another almost never holds
    /// a number this machine's ladder contains. Exact matching sent all of those
    /// to index 0, which is "GPU only": an operator who had asked for 20 threads
    /// reopened the panel and found none selected.
    #[test]
    fn restoring_a_saved_count_lands_on_the_nearest_rung_not_on_gpu_only() {
        let presets = cpu_presets_for(32);
        // Exact hits still hit exactly.
        for (i, preset) in presets.iter().enumerate() {
            assert_eq!(cpu_idx_for_supervene(&presets, preset.supervene), Some(i));
        }
        // 20 is what the shipped ryzen9 preset file asks for. It is not a rung.
        let idx = cpu_idx_for_supervene(&presets, 20).unwrap();
        assert_ne!(idx, 0, "a real thread count must never restore as GPU only");
        assert_eq!(presets[idx].supervene, 16);
        // A count larger than this machine lands on the top rung, not on zero.
        let idx = cpu_idx_for_supervene(&presets, 999).unwrap();
        assert_eq!(presets[idx].supervene, 32);
        // And zero still means zero.
        assert_eq!(cpu_idx_for_supervene(&presets, 0), Some(0));
    }

    /// HACD cannot mine on "GPU only", so the panel substitutes a rung. It used
    /// to substitute the smallest one that had any threads. It substitutes the
    /// machine now.
    #[test]
    fn a_hacd_operator_pushed_off_gpu_only_lands_on_the_whole_machine() {
        let presets = cpu_presets();
        let idx = hacd_default_idx(&presets).expect("a HACD rung exists");
        assert!(presets[idx].supervene > 0);
        assert_eq!(presets[idx].supervene, app::cpu_threads::hacd_threads());
        // Never the smallest rung, on any machine with more than one rung of
        // threads to choose between.
        let smallest = presets.iter().position(|p| p.supervene > 0).unwrap();
        if presets.len() > 3 {
            assert_ne!(idx, smallest);
        }
    }

    /// Small machines must not end up with a one-entry picker or with rungs that
    /// exceed the CPU.
    #[test]
    fn a_small_machine_still_gets_a_usable_picker() {
        let two = cpu_presets_for(2);
        assert_eq!(
            two.iter().map(|p| p.supervene).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        let four = cpu_presets_for(4);
        assert_eq!(
            four.iter().map(|p| p.supervene).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
        // available_parallelism failing is reported as 1 logical CPU.
        let one = cpu_presets_for(1);
        assert_eq!(
            one.iter().map(|p| p.supervene).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn rx7900xtx_max_wg_high() {
        let t = resolve_panel_tuning(&gpu("rx7900xtx"), EfficiencyMode::Max);
        assert!(t.work_groups >= 1024);
    }

    /// The tuning crate drives its limit and auto-tune tests over
    /// `gpu_arch::PANEL_GPU_PRESETS`. If this list and that one drift, a card
    /// added here ships with a search space nobody ever checked.
    #[test]
    fn the_preset_list_matches_the_one_the_tuning_tests_are_driven_over() {
        let here: Vec<(&str, &str, u8)> = gpu_presets()
            .into_iter()
            .filter(|g| g.slug != "none")
            .map(|g| (g.slug, g.profile, g.vram_gb))
            .collect();
        assert_eq!(here, gpu_arch::PANEL_GPU_PRESETS.to_vec());
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
        // 64 x 128 is the Profit tier for this card. It replaced 48 x 48, which
        // was doubly wrong: 48 work groups is a measured scheduling dip (32 CUs
        // at 2 groups each leave a half empty tail on odd multiples of 32), and
        // 48 units left the card starved on a kernel that is latency bound.
        assert_eq!(
            (profile.as_str(), work_groups, unit_size),
            (safe.profile, 64, 128)
        );
        assert_ne!((work_groups, unit_size), (1536, 96));
        assert_ne!(work_groups, 48, "48 work groups is a measured dip");
        assert_ne!(work_groups, 96, "96 work groups is a measured dip");
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
