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

    // gfx1201 does not get its launch shape from the generic tier scaling,
    // because that scaling lands on the two worst values this card has.
    //
    // 32 CUs host 2 work groups each, so a count that is an odd multiple of 32
    // leaves a half empty scheduling tail. A measured sweep dips at exactly 48
    // and 96 and nowhere else, and those were the two the scaling produced:
    // Profit resolved to 48 and, once the unit_size ceiling was raised to the
    // measured optimum, Max resolved to 96.
    //
    // Measured on an RX 9070 XT at repeat 16, fixed corpus, 0.5% noise floor,
    // each shape proven byte identical to the shipped one against the CPU
    // oracle before its number was believed:
    //
    //     48 x 256 x 48  (shipped)  19.13 MH/s
    //     64 x 256 x 192            28.80 MH/s   +50.4%
    //
    // The kernel is latency bound here, not busy, so what helps is nonces in
    // flight; at a matched batch, unit_size buys that about 11% more cheaply
    // than work_groups. Hence one work-group count for all three tiers and a
    // unit_size that carries the tier.
    //
    // Power at the top shape is NOT yet measured. Eco and Profit stay well below
    // it deliberately.
    if limits.is_experimental() {
        let max_tier = ArchLimits::panel_max_tier(panel_slug);
        let base_tier = profile_tier(base_profile);
        let min_tier = min_profile_tier_for_mode(mode);
        let target_tier = (base_tier + mode_tier_offset(mode)).clamp(min_tier, max_tier);
        let unit_size = match mode {
            EfficiencyMode::Eco => 64,
            EfficiencyMode::Profit => 128,
            EfficiencyMode::Max => 192,
        };
        return ResolvedPanelTuning {
            profile: tier_profile_for_vendor(vendor, target_tier),
            work_groups: 64,
            unit_size: unit_size.min(limits.max_unit_size()),
        };
    }

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
    fn rx9070xt_tops_out_at_the_measured_optimum() {
        let t = resolve_panel_tuning("rx9070xt", "amd_performance", 16, EfficiencyMode::Max);
        assert_eq!((t.work_groups, t.unit_size), (64, 192));
    }

    /// The three tiers must differ, and none of them may land on a shape this
    /// card is bad at.
    ///
    /// 32 CUs host 2 work groups each, so an odd multiple of 32 leaves a half
    /// empty scheduling tail. A measured sweep dips at exactly 48 and 96. Before
    /// this table existed the generic scaling produced both: Profit sat on 48
    /// for every shipped build, and raising the unit_size ceiling to the
    /// measured optimum moved Max onto 96.
    #[test]
    fn rx9070xt_modes_differ_and_avoid_the_pathological_shapes() {
        let eco = resolve_panel_tuning("rx9070xt", "amd_balanced", 16, EfficiencyMode::Eco);
        let profit = resolve_panel_tuning("rx9070xt", "amd_balanced", 16, EfficiencyMode::Profit);
        let max = resolve_panel_tuning("rx9070xt", "amd_balanced", 16, EfficiencyMode::Max);

        assert_eq!((eco.work_groups, eco.unit_size), (64, 64));
        assert_eq!((profit.work_groups, profit.unit_size), (64, 128));
        assert_eq!((max.work_groups, max.unit_size), (64, 192));

        // Distinct, and ordered by how hard they drive the card.
        assert!(eco.unit_size < profit.unit_size);
        assert!(profit.unit_size < max.unit_size);

        for t in [&eco, &profit, &max] {
            assert_ne!(t.work_groups, 48, "48 work groups is a measured dip");
            assert_ne!(t.work_groups, 96, "96 work groups is a measured dip");
            assert_eq!(
                t.work_groups % 64,
                0,
                "work groups must divide evenly across 32 CUs at 2 groups each"
            );
        }
    }

    #[test]
    fn rx7900xtx_allows_high_wg() {
        let t = resolve_panel_tuning("rx7900xtx", "amd_max", 24, EfficiencyMode::Max);
        assert!(t.work_groups >= 1024);
    }

    /// The explicit tier table added for the RX 9070 XT must not have moved any
    /// other card off the generic scaling.
    ///
    /// The check recomputes the generic path here rather than calling the branch
    /// under test, so a change that quietly widened the `is_experimental()`
    /// predicate would be caught by the numbers no longer matching, not merely
    /// by the predicate agreeing with itself.
    #[test]
    fn the_explicit_tier_table_is_rx9070xt_only() {
        use crate::gpu_arch::PANEL_GPU_PRESETS;

        for (slug, base_profile, vram_gb) in PANEL_GPU_PRESETS {
            for mode in [
                EfficiencyMode::Eco,
                EfficiencyMode::Profit,
                EfficiencyMode::Max,
            ] {
                let got = resolve_panel_tuning(slug, base_profile, vram_gb, mode);
                if slug == "rx9070xt" {
                    // One work-group count for all three tiers, unit_size carries
                    // the tier, and the top is the measured optimum.
                    assert_eq!(got.work_groups, 64, "{slug} {mode:?}");
                    let expected_us = match mode {
                        EfficiencyMode::Eco => 64,
                        EfficiencyMode::Profit => 128,
                        EfficiencyMode::Max => 192,
                    };
                    assert_eq!(got.unit_size, expected_us, "{slug} {mode:?}");
                    continue;
                }

                // The generic path, recomputed independently.
                let vendor = profile_vendor(base_profile);
                let max_tier = ArchLimits::panel_max_tier(slug);
                let target_tier = (profile_tier(base_profile) + mode_tier_offset(mode))
                    .clamp(min_profile_tier_for_mode(mode), max_tier);
                let profile = tier_profile_for_vendor(vendor, target_tier);
                let (wg, us) = bounded_profile_tuning(
                    profile,
                    ArchLimits::for_panel_slug(slug).panel_min_wg,
                    ArchLimits::panel_max_work_groups(slug, vram_gb),
                    ArchLimits::panel_max_unit_size(slug),
                    max_tier,
                );
                assert_eq!(got.profile, profile, "{slug} {mode:?} profile");
                assert_eq!(
                    (got.work_groups, got.unit_size),
                    (wg, us),
                    "{slug} {mode:?} left the generic tier scaling"
                );
                // And nothing outside gfx1201 may reach the raised ceiling,
                // neither in what it resolves to nor in what it is allowed.
                assert_eq!(
                    ArchLimits::panel_max_unit_size(slug),
                    128,
                    "{slug} was given the gfx1201 unit_size ceiling"
                );
                assert!(got.unit_size <= 128, "{slug} {mode:?} us {}", got.unit_size);
                assert!(got.work_groups >= 256, "{slug} {mode:?}");
            }
        }
    }

    /// The no-GPU entry still resolves to nothing at all, on every mode.
    #[test]
    fn the_none_slug_writes_no_tuning() {
        for mode in [
            EfficiencyMode::Eco,
            EfficiencyMode::Profit,
            EfficiencyMode::Max,
        ] {
            let t = resolve_panel_tuning("none", "", 0, mode);
            assert_eq!((t.profile, t.work_groups, t.unit_size), ("", 0, 0));
        }
    }
}
