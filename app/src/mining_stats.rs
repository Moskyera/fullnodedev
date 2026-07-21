//! Mining stats snapshot build + JSON write (shared by block and diamond workers).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use basis::difficulty::rates_to_show;

use crate::efficiency::{EfficiencyConf, atomic_write_private};
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
    _profile: &str,
    active_cpu: u32,
    diamond_number: u32,
    diamond_best: &str,
    paused: bool,
    _configured_work_groups: u32,
    _oom_work_groups: u32,
    _thermal_cap_work_groups: u32,
    _effective_work_groups: u32,
    _gpu_hashrate_hps: f64,
    cpu_hashrate_hps: f64,
) -> MiningStatsSnapshot {
    // HACD is CPU/full-node mining. Never attribute GPU power, tuning or hash
    // rate to a diamond snapshot, even if an old config still contains them.
    let watts = active_cpu as f64 * eff.cpu_watts_per_thread;
    let kh_per_j = if watts > 0.0 {
        hashrate / watts / 1000.0
    } else {
        0.0
    };
    let daily_cost = watts * 24.0 / 1000.0 * eff.power_cost_kwh;
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
        gpu_profile: String::new(),
        configured_work_groups: 0,
        oom_work_groups: 0,
        thermal_cap_work_groups: 0,
        effective_work_groups: 0,
        gpu_hashrate_hps: 0.0,
        cpu_hashrate_hps,
        gpu_hashrate_display: rates_to_show(0.0),
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
    if let Ok(json) = serde_json::to_vec_pretty(stats) {
        let _ = atomic_write_private(Path::new(path), &json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::efficiency::EfficiencyMode;

    fn efficiency() -> EfficiencyConf {
        EfficiencyConf {
            mode: EfficiencyMode::Profit,
            power_cost_kwh: 0.25,
            gpu_watts: 300.0,
            cpu_watts_per_thread: 8.0,
            hac_price: 0.0,
            dynamic_supervene: true,
            supervene_min: 1,
            supervene_max: 8,
            oom_fallback: true,
            max_temp_c: 85,
            throttle_workgroups: 64,
            thermal_file: String::new(),
            idle_start_hour: 255,
            idle_end_hour: 255,
            pause_if_unprofitable: false,
            benchmark_seconds: 0,
            benchmark_fine_sweep: true,
            thermal_gpu_index: 0,
            stats_file: String::new(),
        }
    }

    #[test]
    fn stats_write_is_atomic_and_replaces_existing_snapshot() {
        let path = std::env::temp_dir().join(format!(
            "hacash-stats-atomic-{}-{}.json",
            std::process::id(),
            unix_ms_now()
        ));
        let mut stats = MiningStatsSnapshot {
            status: "first".to_string(),
            updated_unix_ms: unix_ms_now(),
            ..Default::default()
        };
        write_mining_stats(path.to_str().unwrap(), &stats);
        stats.status = "second".to_string();
        write_mining_stats(path.to_str().unwrap(), &stats);

        let saved: MiningStatsSnapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.status, "second");
        let parent = path.parent().unwrap();
        let prefix = format!(".{}.autotune-", path.file_name().unwrap().to_string_lossy());
        assert_eq!(
            std::fs::read_dir(parent)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
                .count(),
            0
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn hacd_snapshot_is_cpu_only_even_with_legacy_gpu_inputs() {
        let stats = build_diamond_mining_stats(
            1_000_000.0,
            &efficiency(),
            "amd_max",
            4,
            999,
            "WTYUIA",
            false,
            128,
            64,
            32,
            16,
            900_000.0,
            1_000_000.0,
        );
        assert_eq!(stats.watts, 32.0);
        assert!((stats.daily_cost_eur - 0.192).abs() < 0.000_001);
        assert_eq!(stats.gpu_hashrate_hps, 0.0);
        assert_eq!(stats.configured_work_groups, 0);
        assert_eq!(stats.effective_work_groups, 0);
        assert!(stats.gpu_profile.is_empty());
        assert_eq!(stats.cpu_hashrate_hps, 1_000_000.0);
        assert_eq!(stats.diamond_number, 999);
    }
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
