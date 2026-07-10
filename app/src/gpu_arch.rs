//! GPU vendor / architecture detection for OpenCL tuning and kernel compile flags.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuVendor {
    Amd,
    Nvidia,
    Intel,
    Unknown,
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
    if v.contains("nvidia") || n.contains("geforce") || n.contains("rtx ") || n.contains("gtx ")
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
    for token in ["rtx 5090", "rtx 5080", "rtx 5070", "rtx 5060", "rtx 4090", "rtx 4080", "rtx 4070", "rtx 4060", "rtx 3090", "rtx 3080", "rtx 3070", "rtx 3060", "rx 7900", "rx 7800", "rx 7700", "rx 7600", "rx 6900", "rx 6800", "rx 6700", "rx 6600"] {
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
    tune_workgroups(requested, compute_units, vendor, ArchLimits::for_slug("gfx1100"))
}

/// Apply arch limits and CU scaling without forcing a 256 WG floor on RDNA4.
pub fn tune_workgroups(
    requested: u32,
    compute_units: u32,
    vendor: GpuVendor,
    limits: ArchLimits,
) -> u32 {
    if limits.is_experimental() {
        let cap = limits.workgroups_cap(requested.max(limits.panel_min_wg), 1);
        let mut wg = requested.max(limits.panel_min_wg).min(cap);
        wg = (wg / 32).max(1) * 32;
        return wg.clamp(limits.panel_min_wg, cap);
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

    /// RDNA4 / RX 9070 XT — kernel update pending; special OOM + panel treatment.
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
            "rx6600" | "rx7600" | "rtx3060" | "rtx4060" | "rtx5060" => 2,
            "rx6700xt" | "rtx3070" | "rtx4070" | "rtx5070" | "rtx5080" => 3,
            "rx6800xt" | "rx7900xt" | "rx7900xtx" | "rtx4090" | "rtx5090" => 4,
            "rx9070xt" => 3,
            _ => 4,
        }
    }

    /// Panel preset slug → max unit_size (live gfx1201/RDNA4 stable path).
    pub fn panel_max_unit_size(panel_slug: &str) -> u32 {
        if Self::for_panel_slug(panel_slug).is_experimental() {
            64
        } else {
            128
        }
    }

    /// Panel preset slug → max work_groups before profile tuning.
    pub fn panel_max_work_groups(panel_slug: &str, vram_gb: u8) -> u32 {
        let limits = Self::for_panel_slug(panel_slug);
        if limits.is_experimental() {
            return 64;
        }
        match panel_slug {
            "rx6600" | "rx7600" | "rtx3060" | "rtx4060" | "rtx5060" => 1024,
            "rx6700xt" | "rtx3070" | "rtx4070" | "rtx5070" => 1536,
            "rtx5080" => 1792,
            "rx6800xt" | "rx7900xt" => 2048,
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
}