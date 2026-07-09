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
}