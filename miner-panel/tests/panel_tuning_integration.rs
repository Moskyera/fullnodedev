use app::efficiency::EfficiencyMode;
use app::panel_tuning;

#[test]
fn rx9070xt_panel_caps_work_groups() {
    let t =
        panel_tuning::resolve_panel_tuning("rx9070xt", "amd_performance", 16, EfficiencyMode::Max);
    assert_eq!(t.work_groups, 64);
    assert_eq!(t.unit_size, 64);
}

#[test]
fn rx7900xtx_panel_allows_high_work_groups() {
    let t = panel_tuning::resolve_panel_tuning("rx7900xtx", "amd_max", 24, EfficiencyMode::Max);
    assert!(t.work_groups >= 1024);
}
