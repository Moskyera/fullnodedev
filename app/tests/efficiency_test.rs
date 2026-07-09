use app::poworker::PoWorkConf;
use std::collections::HashMap;
use sys::IniObj;

#[test]
fn efficiency_section_loads_defaults() {
    let cnf = PoWorkConf::test_defaults("127.0.0.1:8080".to_string(), 4, 1024);
    assert_eq!(cnf.efficiency.power_cost_kwh, 0.15);
    assert_eq!(cnf.gpu_profile, "amd_profit");
}

#[test]
fn eco_mode_selects_eco_profile() {
    let mut ini: IniObj = HashMap::new();
    let mut eff = HashMap::new();
    eff.insert("mode".to_string(), Some("eco".to_string()));
    ini.insert("efficiency".to_string(), eff);
    let cnf = PoWorkConf::new(&ini);
    assert_eq!(cnf.gpu_profile, "amd_eco");
    assert_eq!(cnf.workgroups, 768);
}

#[test]
fn benchmark_tuned_workgroups_are_preserved() {
    let mut ini: IniObj = HashMap::new();
    let mut gpu = HashMap::new();
    gpu.insert("gpu_profile".to_string(), Some("amd_max".to_string()));
    gpu.insert("work_groups".to_string(), Some("2000".to_string()));
    gpu.insert("unit_size".to_string(), Some("96".to_string()));
    ini.insert("gpu".to_string(), gpu);
    let cnf = PoWorkConf::new(&ini);
    assert_eq!(cnf.workgroups, 2000);
    assert_eq!(cnf.unitsize, 96);
}