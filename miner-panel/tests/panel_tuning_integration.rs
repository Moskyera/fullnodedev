use app::efficiency::EfficiencyMode;
use app::panel_tuning;

/// The panel and the worker must agree on this card's shape, end to end.
///
/// Work groups stay capped at 64, which was never the limit. The unit_size
/// ceiling was: at a matched batch size, 64 x 256 x 192 measured 28.80 MH/s
/// against 25.85 for 256 x 256 x 48, so the same nonces arranged the other way
/// are worth about 11% less. Against the 48 x 256 x 48 the panel used to write,
/// this is 19.13 -> 28.80, +50.4% on a 0.5% noise floor, proven byte identical
/// against the CPU oracle before the number was believed.
#[test]
fn rx9070xt_panel_writes_the_measured_shape() {
    let t =
        panel_tuning::resolve_panel_tuning("rx9070xt", "amd_performance", 16, EfficiencyMode::Max);
    assert_eq!((t.work_groups, t.unit_size), (64, 192));
    assert_ne!(t.work_groups, 48, "48 work groups is a measured dip");
    assert_ne!(t.work_groups, 96, "96 work groups is a measured dip");
}

#[test]
fn rx7900xtx_panel_allows_high_work_groups() {
    let t = panel_tuning::resolve_panel_tuning("rx7900xtx", "amd_max", 24, EfficiencyMode::Max);
    assert!(t.work_groups >= 1024);
}
