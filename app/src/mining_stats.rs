//! Mining stats snapshot build + JSON write (shared by block and diamond workers).

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use basis::difficulty::rates_to_show;

use crate::efficiency::EfficiencyConf;
use crate::mining_runtime::MiningRuntimeState;

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct MiningStatsSnapshot {
    pub status: String,
    pub hashrate_hps: f64,
    pub hashrate_display: String,
    pub watts: f64,
    pub kh_per_j: f64,
    pub hac_per_day: f64,
    pub network_pct: f64,
    pub daily_cost_eur: f64,
    pub daily_revenue_eur: f64,
    pub daily_net_eur: f64,
    pub height: u64,
    pub gpu_profile: String,
    #[serde(default)]
    pub configured_work_groups: u32,
    #[serde(default)]
    pub oom_work_groups: u32,
    #[serde(default)]
    pub thermal_cap_work_groups: u32,
    #[serde(default)]
    pub effective_work_groups: u32,
    pub gpu_hashrate_hps: f64,
    pub cpu_hashrate_hps: f64,
    pub gpu_hashrate_display: String,
    pub active_cpu_threads: u32,
    pub paused_unprofitable: bool,
    pub mining_kind: String,
    pub diamond_number: u32,
    pub diamond_best: String,
    pub updated_unix_ms: u64,
}

pub struct BatchAggregate {
    pub hashrate: f64,
    pub hac_per_day: f64,
    pub network_pct: f64,
    pub height: u64,
    pub gpu_hashrate: f64,
    pub cpu_hashrate: f64,
    pub paused: bool,
}

pub fn emit_from_batch_aggregate(
    agg: &BatchAggregate,
    eff: &EfficiencyConf,
    profile: &str,
    active_cpu: u32,
    configured_work_groups: u32,
    runtime: &MiningRuntimeState,
    mining_kind: &str,
    diamond_number: u32,
    diamond_best: &str,
    stats_path: &str,
) {
    let oom_wg = runtime.oom_work_groups();
    let thermal = runtime.thermal_workgroups_cap().unwrap_or(0);
    let effective = runtime.effective_work_groups();
    let stats = if mining_kind == "hacd" {
        build_diamond_mining_stats(
            agg.hashrate,
            eff,
            profile,
            active_cpu,
            diamond_number,
            diamond_best,
            agg.paused,
            configured_work_groups,
            oom_wg,
            thermal,
            effective,
            agg.gpu_hashrate,
            agg.cpu_hashrate,
        )
    } else {
        build_mining_stats(
            agg.hashrate,
            agg.hac_per_day,
            agg.network_pct,
            eff,
            profile,
            active_cpu,
            agg.height,
            agg.paused,
            configured_work_groups,
            oom_wg,
            thermal,
            effective,
            agg.gpu_hashrate,
            agg.cpu_hashrate,
        )
    };
    write_mining_stats(stats_path, &stats);
}

pub fn build_mining_stats(
    hashrate: f64,
    hac_per_day: f64,
    network_pct: f64,
    eff: &EfficiencyConf,
    profile: &str,
    active_cpu: u32,
    height: u64,
    paused: bool,
    configured_work_groups: u32,
    oom_work_groups: u32,
    thermal_cap_work_groups: u32,
    effective_work_groups: u32,
    gpu_hashrate_hps: f64,
    cpu_hashrate_hps: f64,
) -> MiningStatsSnapshot {
    let gpu_w = eff.estimate_gpu_watts(profile);
    let watts = gpu_w + active_cpu as f64 * eff.cpu_watts_per_thread;
    let kh_per_j = if watts > 0.0 {
        hashrate / watts / 1000.0
    } else {
        0.0
    };
    let daily_cost = eff.daily_power_cost_eur(profile, active_cpu);
    let daily_revenue = hac_per_day * eff.hac_price;
    let daily_net = daily_revenue - daily_cost;
    let status = if paused {
        "paused".to_string()
    } else if hashrate > 0.0 {
        "mining".to_string()
    } else {
        "idle".to_string()
    };
    MiningStatsSnapshot {
        status,
        hashrate_hps: hashrate,
        hashrate_display: rates_to_show(hashrate),
        watts,
        kh_per_j,
        hac_per_day,
        network_pct,
        daily_cost_eur: daily_cost,
        daily_revenue_eur: daily_revenue,
        daily_net_eur: daily_net,
        height,
        gpu_profile: profile.to_string(),
        configured_work_groups,
        oom_work_groups,
        thermal_cap_work_groups,
        effective_work_groups,
        gpu_hashrate_hps,
        cpu_hashrate_hps,
        gpu_hashrate_display: rates_to_show(gpu_hashrate_hps),
        active_cpu_threads: active_cpu,
        paused_unprofitable: paused,
        mining_kind: "hac".to_string(),
        diamond_number: 0,
        diamond_best: String::new(),
        updated_unix_ms: unix_ms_now(),
    }
}

pub fn build_diamond_mining_stats(
    hashrate: f64,
    eff: &EfficiencyConf,
    profile: &str,
    active_cpu: u32,
    diamond_number: u32,
    diamond_best: &str,
    paused: bool,
    configured_work_groups: u32,
    oom_work_groups: u32,
    thermal_cap_work_groups: u32,
    effective_work_groups: u32,
    gpu_hashrate_hps: f64,
    cpu_hashrate_hps: f64,
) -> MiningStatsSnapshot {
    let gpu_w = eff.estimate_gpu_watts(profile);
    let watts = gpu_w + active_cpu as f64 * eff.cpu_watts_per_thread;
    let kh_per_j = if watts > 0.0 {
        hashrate / watts / 1000.0
    } else {
        0.0
    };
    let daily_cost = eff.daily_power_cost_eur(profile, active_cpu);
    let status = if paused {
        "paused".to_string()
    } else if hashrate > 0.0 {
        "mining".to_string()
    } else {
        "idle".to_string()
    };
    MiningStatsSnapshot {
        status,
        hashrate_hps: hashrate,
        hashrate_display: rates_to_show(hashrate),
        watts,
        kh_per_j,
        hac_per_day: 0.0,
        network_pct: 0.0,
        daily_cost_eur: daily_cost,
        daily_revenue_eur: 0.0,
        daily_net_eur: -daily_cost,
        height: diamond_number as u64,
        gpu_profile: profile.to_string(),
        configured_work_groups,
        oom_work_groups,
        thermal_cap_work_groups,
        effective_work_groups,
        gpu_hashrate_hps,
        cpu_hashrate_hps,
        gpu_hashrate_display: rates_to_show(gpu_hashrate_hps),
        active_cpu_threads: active_cpu,
        paused_unprofitable: paused,
        mining_kind: "hacd".to_string(),
        diamond_number,
        diamond_best: diamond_best.to_string(),
        updated_unix_ms: unix_ms_now(),
    }
}

pub fn write_mining_stats(path: &str, stats: &MiningStatsSnapshot) {
    if path.is_empty() {
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(stats) {
        let _ = fs::write(path, json);
    }
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}