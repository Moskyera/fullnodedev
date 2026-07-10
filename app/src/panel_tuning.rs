//! Single entry point for panel → poworker OpenCL tuning resolution.

use crate::efficiency::{
    min_profile_tier_for_mode, profile_tier, profile_tuning, tier_profile_for_vendor,
    EfficiencyMode,
};
use crate::gpu_arch::{ArchLimits, GpuVendor};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPanelTuning {
    pub profile: &'static str,
    pub work_groups: u32,
    pub unit_size: u32,
}

fn vendor_for_profile(profile: &str) -> GpuVendor {
    if profile.starts_with("nvidia_") {
        GpuVendor::Nvidia
    } else if profile.starts_with("intel_") {
        GpuVendor::Intel
    } else {
        GpuVendor::Amd
    }
}

fn mode_tier_offset(mode: EfficiencyMode) -> i8 {
    match mode {
        EfficiencyMode::Eco => -1,
        EfficiencyMode::Profit => 0,
        EfficiencyMode::Max => 1,
    }
}

/// Resolve profile + work_groups + unit_size from GPU slug, base profile, VRAM, and mode.
pub fn resolve_panel_tuning(
    panel_slug: &str,
    base_profile: &str,
    vram_gb: u8,
    mode: EfficiencyMode,
) -> ResolvedPanelTuning {
    if panel_slug == "none" {
        return ResolvedPanelTuning {
            profile: "",
            work_groups: 0,
            unit_size: 0,
        };
    }

    let limits = ArchLimits::for_panel_slug(panel_slug);
    let vendor = vendor_for_profile(base_profile);
    let base_tier = profile_tier(base_profile);
    let max_tier = ArchLimits::panel_max_tier(panel_slug);
    let min_tier = min_profile_tier_for_mode(mode);
    let target_tier = (base_tier + mode_tier_offset(mode)).clamp(min_tier, max_tier);
    let profile = tier_profile_for_vendor(vendor, target_tier);
    let (mut wg, mut us) = profile_tuning(profile);

    let max_wg = ArchLimits::panel_max_work_groups(panel_slug, vram_gb);
    if max_wg > 0 {
        wg = wg.min(max_wg);
    }
    let max_us = ArchLimits::panel_max_unit_size(panel_slug);
    if max_us > 0 {
        us = us.min(max_us);
    }
    let min_wg = limits.panel_min_wg;

    ResolvedPanelTuning {
        profile,
        work_groups: wg.max(min_wg),
        unit_size: us,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rx9070xt_capped_conservatively() {
        let t = resolve_panel_tuning("rx9070xt", "amd_performance", 16, EfficiencyMode::Max);
        assert_eq!(t.work_groups, 64);
        assert_eq!(t.unit_size, 64);
    }

    #[test]
    fn rx7900xtx_allows_high_wg() {
        let t = resolve_panel_tuning("rx7900xtx", "amd_max", 24, EfficiencyMode::Max);
        assert!(t.work_groups >= 1024);
    }
}