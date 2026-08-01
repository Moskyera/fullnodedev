//! GPU vendor / architecture detection for OpenCL tuning and kernel compile flags.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuVendor {
    Amd,
    Nvidia,
    Intel,
    Unknown,
}

/// Infer the vendor encoded in a named tuning profile.
pub fn profile_vendor(profile: &str) -> GpuVendor {
    if profile.starts_with("nvidia_") {
        GpuVendor::Nvidia
    } else if profile.starts_with("intel_") {
        GpuVendor::Intel
    } else if profile.starts_with("amd_") {
        GpuVendor::Amd
    } else {
        GpuVendor::Unknown
    }
}

impl GpuVendor {
    pub fn prefix(self) -> &'static str {
        match self {
            GpuVendor::Amd => "amd",
            GpuVendor::Nvidia => "nvidia",
            GpuVendor::Intel => "intel",
            GpuVendor::Unknown => "gpu",
        }
    }
}

/// Detect GPU vendor from OpenCL device vendor + name strings.
pub fn detect_vendor(vendor: &str, name: &str) -> GpuVendor {
    let v = vendor.to_lowercase();
    let n = name.to_lowercase();
    if v.contains("nvidia")
        || n.contains("geforce")
        || n.contains("rtx ")
        || n.contains("gtx ")
        || n.contains("quadro")
    {
        return GpuVendor::Nvidia;
    }
    if v.contains("amd")
        || v.contains("advanced micro devices")
        || n.contains("radeon")
        || n.contains("gfx")
    {
        return GpuVendor::Amd;
    }
    if v.contains("intel") || n.contains("arc ") || n.contains("iris") || n.contains("uhd graphics")
    {
        return GpuVendor::Intel;
    }
    GpuVendor::Unknown
}

/// Short architecture slug for kernel binary cache (safe filename fragment).
pub fn arch_slug(name: &str) -> String {
    let n = name.to_lowercase();
    if let Some(idx) = n.find("gfx") {
        let tail: String = n[idx..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if tail.len() >= 5 {
            return tail;
        }
    }
    for model in ["a770", "a750", "a580", "a380", "a310"] {
        if n.contains(model) && n.contains("arc") {
            return format!("arc{model}");
        }
    }
    for token in [
        "rtx 5090", "rtx 5080", "rtx 5070", "rtx 5060", "rtx 4090", "rtx 4080", "rtx 4070",
        "rtx 4060", "rtx 3090", "rtx 3080", "rtx 3070", "rtx 3060", "rx 7900", "rx 7800",
        "rx 7700", "rx 7600", "rx 6900", "rx 6800", "rx 6700", "rx 6600",
    ] {
        if n.contains(token) {
            return token.replace(' ', "");
        }
    }
    n.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .chars()
        .take(24)
        .collect()
}

/// Map generic or cross-vendor profile name to vendor-specific profile.
pub fn normalize_profile(profile: &str, vendor: GpuVendor) -> String {
    let p = profile.trim().to_lowercase();
    let tier = if p.contains("eco") {
        "eco"
    } else if p.contains("balanced") {
        "balanced"
    } else if p.contains("profit") {
        "profit"
    } else if p.contains("max") {
        "max"
    } else if p.contains("performance") || p.contains("perf") {
        "performance"
    } else {
        return profile.to_string();
    };
    format!("{}_{}", vendor.prefix(), tier)
}

/// Scale work_groups from device compute-unit count (waves per CU).
pub fn suggest_workgroups(requested: u32, compute_units: u32, vendor: GpuVendor) -> u32 {
    tune_workgroups(
        requested,
        compute_units,
        vendor,
        ArchLimits::for_slug("gfx1100"),
    )
}

/// Apply arch limits and CU scaling without forcing a 256 WG floor on RDNA4.
pub fn tune_workgroups(
    requested: u32,
    compute_units: u32,
    vendor: GpuVendor,
    limits: ArchLimits,
) -> u32 {
    if limits.is_experimental() {
        // RDNA4 Auto Tune validates exact launch points such as 48 WG.
        // Preserve that measured value instead of silently rounding it down to 32.
        return limits
            .workgroups_cap(requested.max(limits.panel_min_wg), 1)
            .max(limits.panel_min_wg);
    }
    if compute_units == 0 {
        return requested.max(256);
    }
    let waves_per_cu = match vendor {
        GpuVendor::Amd => 64u32,
        GpuVendor::Nvidia => 48,
        GpuVendor::Intel => 32,
        GpuVendor::Unknown => 40,
    };
    let target = compute_units.saturating_mul(waves_per_cu);
    let mut wg = requested.min(target.max(256));
    wg = (wg / 64).max(4) * 64;
    wg.clamp(256, 4096)
}

/// OpenCL compiler `-D` flags for architecture-specific paths.
pub fn compile_defines(vendor: GpuVendor, slug: &str, amd_fast: bool) -> String {
    let mut defs = String::from(" -cl-single-precision-constant");
    match vendor {
        GpuVendor::Amd if amd_fast => defs.push_str(" -DNO_AMD_OPS=0"),
        GpuVendor::Nvidia => {
            defs.push_str(" -DNVIDIA_GPU=1 -DNO_AMD_OPS=1 -cl-denorms-are-zero");
        }
        GpuVendor::Intel => defs.push_str(" -DINTEL_GPU=1 -DNO_AMD_OPS=1"),
        _ => {}
    }
    if slug.starts_with("gfx") {
        defs.push_str(&format!(" -DAMD_GFX_{}=1", slug.to_uppercase()));
    } else if slug.starts_with("rtx") || slug.starts_with("rx") {
        defs.push_str(&format!(" -DGPU_{}=1", slug.to_uppercase()));
    }
    defs
}

/// Per-architecture OpenCL tuning limits (single source of truth).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchLimits {
    pub oom_floor_wg: u32,
    pub init_buffer_floor_wg: u32,
    pub panel_min_wg: u32,
    pub oom_ramp_to_base: bool,
}

impl ArchLimits {
    /// Map panel preset slug (e.g. rx9070xt) or OpenCL arch slug (e.g. gfx1201).
    pub fn for_panel_slug(slug: &str) -> Self {
        Self::for_slug(match slug {
            "rx9070xt" => "gfx1201",
            other => other,
        })
    }

    pub fn for_slug(slug: &str) -> Self {
        if slug == "gfx1201" {
            Self {
                oom_floor_wg: 32,
                init_buffer_floor_wg: 32,
                panel_min_wg: 32,
                oom_ramp_to_base: false,
            }
        } else {
            Self {
                oom_floor_wg: 512,
                init_buffer_floor_wg: 256,
                panel_min_wg: 256,
                oom_ramp_to_base: true,
            }
        }
    }

    /// RDNA4 / RX 9070 XT, validated conservative launch and OOM treatment.
    pub fn is_experimental(&self) -> bool {
        !self.oom_ramp_to_base && self.oom_floor_wg == 32
    }

    /// Cap work_groups for experimental arches (matches panel_max_work_groups).
    pub fn workgroups_cap(&self, requested: u32, _amd_icd_count: usize) -> u32 {
        if self.is_experimental() {
            requested.min(64)
        } else {
            requested
        }
    }

    /// Drain AMD queue after each batch when RDNA4 or duplicate ICDs are present.
    pub fn needs_amd_queue_finish(slug: &str, duplicate_amd_icd: bool) -> bool {
        Self::for_slug(slug).is_experimental() || duplicate_amd_icd
    }

    /// Panel preset slug → max profile tier (0..=4).
    pub fn panel_max_tier(panel_slug: &str) -> i8 {
        match panel_slug {
            "rx6600" | "rx7600" | "rtx3060" | "rtx4060" | "rtx5060" | "arc_a380" => 2,
            "rx6700xt" | "rtx3070" | "rtx4070" | "rtx5070" | "rtx5080" | "arc_a750" => 3,
            "rx6800xt" | "rx7900xt" | "rx7900xtx" | "rtx4090" | "rtx5090" | "arc_a770" => 4,
            "rx9070xt" => 3,
            _ => 4,
        }
    }

    /// Panel preset slug → max unit_size (live gfx1201/RDNA4 stable path).
    ///
    /// 192 for gfx1201, and the number is measured rather than chosen.
    ///
    /// The kernel is latency bound on this card, not busy: 256 VGPRs, spill to
    /// scratch, tables read from `__constant`, LDS unused. It is starved of work
    /// in flight, and `unit_size` feeds it far more cheaply than `work_groups`
    /// does. At a matched batch of 3,145,728 nonces, 64 groups x 192 units gives
    /// 28.80 MH/s against 25.85 for 256 groups x 48, so the same nonces arranged
    /// the other way are worth about 11% less.
    ///
    /// Against the shipping 48 x 256 x 48 this is 19.13 -> 28.80 MH/s, +50.4% on
    /// a 0.5% noise floor, bracketed by opening and closing controls that agreed
    /// to 0.26%. Byte equivalence was proven at the new shape before the number
    /// was believed: 1,638,950 hashes compared against the CPU oracle, zero
    /// mismatches, every one of the 16 algorithms about 20,600 rounds.
    ///
    /// Note the shipped 48 was pathological. 32 CUs host 2 groups each, so 48
    /// and 96 leave a half empty scheduling tail while 64, 128 and 192 divide
    /// evenly, which is why the sweep dips at 96 and not elsewhere.
    ///
    /// `is_experimental()` is true only for gfx1201, and `workgroups_cap` holds
    /// groups at 64 there, so the largest batch this permits is 64 x 256 x 192 =
    /// 113 MB of device state. It cannot reach the multi gigabyte allocations a
    /// raised work-group cap would.
    pub fn max_unit_size(&self) -> u32 {
        if self.is_experimental() { 192 } else { 128 }
    }

    pub fn panel_max_unit_size(panel_slug: &str) -> u32 {
        Self::for_panel_slug(panel_slug).max_unit_size()
    }

    /// Panel preset slug → max work_groups before profile tuning.
    pub fn panel_max_work_groups(panel_slug: &str, vram_gb: u8) -> u32 {
        let limits = Self::for_panel_slug(panel_slug);
        if limits.is_experimental() {
            return 64;
        }
        match panel_slug {
            "rx6600" | "rx7600" | "rtx3060" | "rtx4060" | "rtx5060" | "arc_a380" => 1024,
            "rx6700xt" | "rtx3070" | "rtx4070" | "rtx5070" | "arc_a750" => 1536,
            "rtx5080" => 1792,
            "rx6800xt" | "rx7900xt" | "arc_a770" => 2048,
            "rx7900xtx" => 4096,
            "rtx4090" | "rtx5090" => 3584,
            _ => match vram_gb {
                0..=8 => 1024,
                9..=12 => 1536,
                13..=16 => 2048,
                17..=24 => 3072,
                _ => 4096,
            },
        }
    }
}

/// Panel preset slug → minimum work_groups written to ini.
pub fn panel_min_work_groups(gpu_slug: &str) -> u32 {
    ArchLimits::for_panel_slug(gpu_slug).panel_min_wg
}

/// Every card the panel can be set to, as (slug, shipped base profile, VRAM GB).
///
/// The panel owns the labels and the watts; this is the part the tuning code
/// needs, kept here so that limits, panel resolution and the auto-tuner can all
/// be tested over the same list instead of three lists that drift. The panel's
/// own preset table is asserted equal to this one, so adding a card there
/// without adding it here fails a test rather than shipping an untested card.
///
/// "none" is deliberately absent: it is the no-GPU entry and has no limits.
pub const PANEL_GPU_PRESETS: [(&str, &str, u8); 19] = [
    ("rx6600", "amd_balanced", 8),
    ("rx7600", "amd_balanced", 8),
    ("rx6700xt", "amd_performance", 12),
    ("rx6800xt", "amd_performance", 16),
    ("rx7900xt", "amd_performance", 20),
    ("rx7900xtx", "amd_max", 24),
    ("rx9070xt", "amd_balanced", 16),
    ("rtx3060", "nvidia_balanced", 8),
    ("rtx4060", "nvidia_balanced", 8),
    ("rtx3070", "nvidia_profit", 12),
    ("rtx4070", "nvidia_performance", 12),
    ("rtx4090", "nvidia_max", 24),
    ("rtx5060", "nvidia_balanced", 8),
    ("rtx5070", "nvidia_performance", 12),
    ("rtx5080", "nvidia_performance", 16),
    ("rtx5090", "nvidia_max", 32),
    ("arc_a380", "intel_balanced", 6),
    ("arc_a750", "intel_performance", 8),
    ("arc_a770", "intel_performance", 16),
];

/// Every architecture slug `arch_slug()` can hand `ArchLimits::for_slug`, one
/// per family the detection code names, plus the generic fallback shape.
///
/// Used by the tests that have to prove a change was confined to one card.
pub const KNOWN_ARCH_SLUGS: [&str; 22] = [
    // AMD, as the OpenCL driver reports them.
    "gfx1201", "gfx1200", "gfx1151", "gfx1100", "gfx1101", "gfx1102", "gfx1030", "gfx1031",
    "gfx1032", "gfx1010", "gfx906", "gfx900", // Intel Arc, from the model table.
    "arca770", "arca750", "arca580", "arca380", "arca310",
    // NVIDIA and AMD board names, from the token table.
    "rtx5090", "rtx4090", "rtx3060", "rx7900", "rx6600",
];

/// Sanitize device name for use in binary cache filenames.
pub fn safe_device_filename(device_name: &str) -> String {
    device_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_vendor_is_detected() {
        assert_eq!(profile_vendor("amd_profit"), GpuVendor::Amd);
        assert_eq!(profile_vendor("nvidia_max"), GpuVendor::Nvidia);
        assert_eq!(profile_vendor("intel_balanced"), GpuVendor::Intel);
        assert_eq!(profile_vendor("custom"), GpuVendor::Unknown);
    }

    #[test]
    fn detects_nvidia() {
        assert_eq!(
            detect_vendor("NVIDIA Corporation", "NVIDIA GeForce RTX 4070"),
            GpuVendor::Nvidia
        );
    }

    #[test]
    fn detects_amd() {
        assert_eq!(
            detect_vendor("Advanced Micro Devices, Inc.", "gfx1100"),
            GpuVendor::Amd
        );
    }

    #[test]
    fn normalizes_profile_for_nvidia() {
        assert_eq!(
            normalize_profile("amd_profit", GpuVendor::Nvidia),
            "nvidia_profit"
        );
    }

    #[test]
    fn arch_slug_rtx() {
        assert!(arch_slug("NVIDIA GeForce RTX 4090").contains("rtx4090"));
    }

    #[test]
    fn arch_slug_intel_arc() {
        assert_eq!(arch_slug("Intel(R) Arc(TM) A770 Graphics"), "arca770");
    }

    #[test]
    fn suggest_workgroups_scales_with_cu() {
        let wg = suggest_workgroups(4096, 64, GpuVendor::Amd);
        assert!(wg >= 256 && wg <= 4096);
        assert!(wg % 64 == 0);
    }

    #[test]
    fn gfx1201_arch_limits() {
        let lim = ArchLimits::for_slug("gfx1201");
        assert_eq!(lim.oom_floor_wg, 32);
        assert_eq!(lim.panel_min_wg, 32);
        assert!(!lim.oom_ramp_to_base);
    }

    #[test]
    fn default_arch_limits_use_256_floor() {
        let lim = ArchLimits::for_slug("gfx1100");
        assert_eq!(lim.oom_floor_wg, 512);
        assert_eq!(lim.panel_min_wg, 256);
        assert!(lim.oom_ramp_to_base);
    }

    #[test]
    fn panel_min_work_groups_rx9070xt() {
        assert_eq!(panel_min_work_groups("rx9070xt"), 32);
        assert_eq!(panel_min_work_groups("rx7900xtx"), 256);
    }

    #[test]
    fn gfx1201_workgroups_cap_with_duplicate_icd() {
        let lim = ArchLimits::for_slug("gfx1201");
        assert_eq!(lim.workgroups_cap(2048, 2), 64);
        assert_eq!(lim.workgroups_cap(2048, 1), 64);
        assert_eq!(lim.workgroups_cap(64, 1), 64);
        assert_eq!(lim.workgroups_cap(32, 1), 32);
    }

    #[test]
    fn gfx1201_tune_workgroups_respects_ini_not_256_floor() {
        let lim = ArchLimits::for_slug("gfx1201");
        assert_eq!(tune_workgroups(32, 32, GpuVendor::Amd, lim), 32);
        assert_eq!(tune_workgroups(48, 32, GpuVendor::Amd, lim), 48);
        assert_eq!(tune_workgroups(64, 32, GpuVendor::Amd, lim), 64);
        assert_eq!(tune_workgroups(128, 32, GpuVendor::Amd, lim), 64);
    }

    #[test]
    fn rx9070xt_panel_slug_maps_to_gfx1201_limits() {
        let lim = ArchLimits::for_panel_slug("rx9070xt");
        assert!(lim.is_experimental());
        assert_eq!(lim.panel_min_wg, 32);
    }

    #[test]
    fn needs_amd_queue_finish_for_gfx1201() {
        assert!(ArchLimits::needs_amd_queue_finish("gfx1201", false));
        assert!(!ArchLimits::needs_amd_queue_finish("gfx1100", false));
        assert!(ArchLimits::needs_amd_queue_finish("gfx1100", true));
    }

    /// The raised unit_size ceiling was measured on one card and must stay on it.
    ///
    /// 192 is the RX 9070 XT's measured optimum. It was never measured anywhere
    /// else, and on a card with a different register file or a different LDS
    /// budget it is a guess that costs VRAM and batch latency. Every other slug
    /// keeps 128, which is what every build before this one used.
    #[test]
    fn the_raised_unit_size_ceiling_is_gfx1201_only() {
        assert_eq!(ArchLimits::for_slug("gfx1201").max_unit_size(), 192);
        assert_eq!(ArchLimits::panel_max_unit_size("rx9070xt"), 192);

        for slug in KNOWN_ARCH_SLUGS {
            let expected = if slug == "gfx1201" { 192 } else { 128 };
            assert_eq!(
                ArchLimits::for_slug(slug).max_unit_size(),
                expected,
                "arch slug {slug}"
            );
        }
        for (slug, _, _) in PANEL_GPU_PRESETS {
            let expected = if slug == "rx9070xt" { 192 } else { 128 };
            assert_eq!(
                ArchLimits::panel_max_unit_size(slug),
                expected,
                "panel slug {slug}"
            );
        }
        // An unrecognised card is not experimental either.
        assert_eq!(ArchLimits::panel_max_unit_size("some_future_card"), 128);
        assert_eq!(ArchLimits::for_slug("").max_unit_size(), 128);
    }

    /// Everything `is_experimental()` gates is likewise one card's: the 32 work
    /// group floor, the 64 work group cap, the refusal to ramp back to base
    /// after an OOM, and the queue drain after every batch.
    #[test]
    fn the_experimental_limits_are_gfx1201_only() {
        for slug in KNOWN_ARCH_SLUGS {
            let limits = ArchLimits::for_slug(slug);
            if slug == "gfx1201" {
                assert!(limits.is_experimental(), "{slug}");
                assert_eq!(limits.panel_min_wg, 32);
                assert_eq!(limits.workgroups_cap(4096, 1), 64);
                assert!(ArchLimits::needs_amd_queue_finish(slug, false));
            } else {
                assert!(!limits.is_experimental(), "{slug}");
                assert_eq!(limits.panel_min_wg, 256, "{slug}");
                assert_eq!(limits.oom_floor_wg, 512, "{slug}");
                assert!(limits.oom_ramp_to_base, "{slug}");
                assert_eq!(limits.workgroups_cap(4096, 1), 4096, "{slug}");
                assert!(!ArchLimits::needs_amd_queue_finish(slug, false), "{slug}");
            }
        }
        for (slug, _, _) in PANEL_GPU_PRESETS {
            assert_eq!(
                ArchLimits::for_panel_slug(slug).is_experimental(),
                slug == "rx9070xt",
                "panel slug {slug}"
            );
        }
    }

    /// `vram_gb` is a live input only on the fallback path. Every named preset
    /// carries its own hard ceiling, so a wrong VRAM reading cannot move it.
    #[test]
    fn vram_moves_the_ceiling_only_for_cards_with_no_preset() {
        for (slug, _, vram) in PANEL_GPU_PRESETS {
            let at_its_own = ArchLimits::panel_max_work_groups(slug, vram);
            for other in [0u8, 2, 4, 6, 8, 12, 16, 24, 32, 48, 255] {
                assert_eq!(
                    ArchLimits::panel_max_work_groups(slug, other),
                    at_its_own,
                    "{slug} moved when told it had {other} GB"
                );
            }
        }
    }

    /// The fallback path is the only one that reads VRAM, and it has to
    /// discriminate: a 4 GB card and a 24 GB card must not get one ceiling.
    #[test]
    fn the_vram_fallback_separates_a_small_card_from_a_large_one() {
        let at = |gb: u8| ArchLimits::panel_max_work_groups("some_future_card", gb);
        assert!(
            at(4) < at(24),
            "4 GB and 24 GB both got {} work groups",
            at(4)
        );
        assert_eq!(at(4), 1024);
        assert_eq!(at(24), 3072);
        assert_eq!(at(32), 4096);
        // Monotone, so more memory is never punished.
        let mut previous = 0;
        for gb in 0u8..=64 {
            let now = at(gb);
            assert!(now >= previous, "{gb} GB dropped to {now} from {previous}");
            previous = now;
        }
        // And the spread is real rather than cosmetic.
        assert!(at(64) >= at(4) * 4);
    }
}
