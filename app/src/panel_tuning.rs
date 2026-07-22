//! Single entry point for panel → poworker OpenCL tuning resolution.

use crate::efficiency::{
    EfficiencyMode, bounded_profile_tuning, min_profile_tier_for_mode, profile_tier,
    tier_profile_for_vendor,
};
use crate::gpu_arch::{ArchLimits, profile_vendor};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPanelTuning {
    pub profile: &'static str,
    pub work_groups: u32,
    pub unit_size: u32,
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
    let vendor = profile_vendor(base_profile);
    let base_tier = profile_tier(base_profile);
    let max_tier = ArchLimits::panel_max_tier(panel_slug);
    let min_tier = min_profile_tier_for_mode(mode);
    let target_tier = (base_tier + mode_tier_offset(mode)).clamp(min_tier, max_tier);
    let profile = tier_profile_for_vendor(vendor, target_tier);
    let max_wg = ArchLimits::panel_max_work_groups(panel_slug, vram_gb);
    let max_us = ArchLimits::panel_max_unit_size(panel_slug);
    let min_wg = limits.panel_min_wg;
    let (wg, us) = bounded_profile_tuning(profile, min_wg, max_wg, max_us, max_tier);

    ResolvedPanelTuning {
        profile,
        work_groups: wg,
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
    fn rx9070xt_modes_use_distinct_safe_launch_sizes() {
        let eco = resolve_panel_tuning("rx9070xt", "amd_balanced", 16, EfficiencyMode::Eco);
        let profit = resolve_panel_tuning("rx9070xt", "amd_balanced", 16, EfficiencyMode::Profit);
        let max = resolve_panel_tuning("rx9070xt", "amd_balanced", 16, EfficiencyMode::Max);
        assert_eq!((eco.work_groups, eco.unit_size), (32, 32));
        assert_eq!((profit.work_groups, profit.unit_size), (48, 48));
        assert_eq!((max.work_groups, max.unit_size), (64, 64));
    }

    #[test]
    fn rx7900xtx_allows_high_wg() {
        let t = resolve_panel_tuning("rx7900xtx", "amd_max", 24, EfficiencyMode::Max);
        assert!(t.work_groups >= 1024);
    }
}
