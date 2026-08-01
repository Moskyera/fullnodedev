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
    /// Total draw of the whole rig: the GPUs plus the CPU threads assisting them.
    ///
    /// The GPU half is a real measurement wherever `gpu_board_power_w` is
    /// present and the configured `gpu_watts` estimate everywhere else. The CPU
    /// half is always an estimate, because nothing here measures CPU package
    /// power. `watts_measured` says which of those two this number is, and it is
    /// the only honest way to read this field.
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
    /// The work-group count the OOM fallback currently ALLOWS, per GPU, taken at
    /// its worst across the devices in this rig.
    ///
    /// It is a count, not a loss. On a card that has never hit an out-of-memory
    /// batch this equals `configured_work_groups`, and a healthy rig therefore
    /// publishes a large positive number here every second it mines. Anything
    /// reading `> 0` as "an OOM clamp happened" is wrong about every rig it will
    /// ever meet; the clamp is the gap, so the only true test is
    /// `oom_allowed_work_groups < configured_work_groups`.
    ///
    /// Serialised under its old key as well, because a stats file written by an
    /// already-installed worker has to keep loading.
    #[serde(default, alias = "oom_work_groups")]
    pub oom_allowed_work_groups: u32,
    /// The thermal cap in force, or 0 when the thermal guard has capped nothing.
    /// Unlike the field above, this one really is absent when it is zero.
    #[serde(default)]
    pub thermal_cap_work_groups: u32,
    #[serde(default)]
    pub effective_work_groups: u32,
    pub gpu_hashrate_hps: f64,
    pub cpu_hashrate_hps: f64,
    pub gpu_hashrate_display: String,
    /// The GPU temperature in Celsius, as read from the sensor this build
    /// already talks to (`thermal_file`, `nvidia-smi`, `rocm-smi`, `amd-smi`).
    ///
    /// `None` where no sensor answered, and that is the whole point of the
    /// Option: a machine with no readable sensor must publish nothing here. A
    /// gauge reading 0 degrees looks like a working sensor on a cold card, and
    /// is the one thing this field must never cause.
    #[serde(default)]
    pub gpu_temp_c: Option<f32>,
    /// Total board power the rig's GPUs are really drawing, in watts, summed
    /// across every card, as read from the card's own sensor.
    ///
    /// `None` where nothing measured it, under exactly the rule `gpu_temp_c`
    /// follows: no sensor, no field, never a zero. It matters more here than it
    /// does for temperature. A gauge reading 0 C looks like a cold card; a cost
    /// figure built on 0 W looks like free electricity, and it would flow
    /// straight into kH/J, the daily cost, and the profitability pause.
    ///
    /// This is BOARD power, the whole card including memory and VRM losses,
    /// which is the number the electricity meter sees. It is not the die-only
    /// ASIC figure, which reads tens of watts lower.
    #[serde(default)]
    pub gpu_board_power_w: Option<f32>,
    /// Whether `watts` is a measurement rather than an estimate, end to end.
    ///
    /// True only when every part of the total came from a sensor: the GPUs
    /// measured, and no CPU-assist threads whose draw could only ever be
    /// estimated. A rig with a measured card AND two assist threads publishes
    /// `gpu_board_power_w` and leaves this false, because two thirds of a truth
    /// is not a measurement and the panel must not label it as one.
    #[serde(default)]
    pub watts_measured: bool,
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
    let oom_wg = runtime.oom_allowed_work_groups();
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
            runtime.gpu_temp_c(),
            runtime.gpu_board_power_w(),
        )
    };
    write_mining_stats(stats_path, &stats);
}

#[allow(clippy::too_many_arguments)]
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
    oom_allowed_work_groups: u32,
    thermal_cap_work_groups: u32,
    effective_work_groups: u32,
    gpu_hashrate_hps: f64,
    cpu_hashrate_hps: f64,
    gpu_temp_c: Option<f32>,
    gpu_board_power_w: Option<f32>,
) -> MiningStatsSnapshot {
    // A measured board draw replaces the configured guess wherever one exists,
    // and everything below is computed from it: efficiency, daily cost, and the
    // margin the profit pause acts on. That is the whole point of measuring.
    let measured_gpu_w = sensor_board_power_w(gpu_board_power_w);
    let gpu_w = measured_gpu_w
        .map(f64::from)
        .unwrap_or_else(|| eff.estimate_gpu_watts(profile));
    let cpu_w = active_cpu as f64 * eff.cpu_watts_per_thread;
    let watts = gpu_w + cpu_w;
    let kh_per_j = if watts > 0.0 {
        hashrate / watts / 1000.0
    } else {
        0.0
    };
    let daily_cost = watts * 24.0 / 1000.0 * eff.power_cost_kwh;
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
        oom_allowed_work_groups,
        thermal_cap_work_groups,
        effective_work_groups,
        gpu_hashrate_hps,
        cpu_hashrate_hps,
        gpu_hashrate_display: rates_to_show(gpu_hashrate_hps),
        gpu_temp_c: sensor_temperature(gpu_temp_c),
        gpu_board_power_w: measured_gpu_w,
        // The CPU term is an estimate, so the total is only a measurement when
        // there is no CPU term at all.
        //
        // Tested on `active_cpu`, not on `cpu_w`. `cpu_w` is
        // `active_cpu * cpu_watts_per_thread`, so an operator who sets
        // `cpu_watts_per_thread = 0` while CPU assist threads are running makes
        // it zero with the CPU draw simply missing from the total, and the label
        // would then call that total "measured". The thread count cannot lie the
        // same way.
        watts_measured: measured_gpu_w.is_some() && active_cpu == 0,
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
    _oom_allowed_work_groups: u32,
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
        oom_allowed_work_groups: 0,
        thermal_cap_work_groups: 0,
        effective_work_groups: 0,
        gpu_hashrate_hps: 0.0,
        cpu_hashrate_hps,
        gpu_hashrate_display: rates_to_show(0.0),
        // HACD mines on the CPU through the full node. There is no GPU under
        // this snapshot, so there is no GPU temperature and no GPU board power
        // to report, and the CPU watts here are the configured per-thread
        // estimate rather than anything measured.
        gpu_temp_c: None,
        gpu_board_power_w: None,
        watts_measured: false,
        active_cpu_threads: active_cpu,
        paused_unprofitable: paused,
        mining_kind: "hacd".to_string(),
        diamond_number,
        diamond_best: diamond_best.to_string(),
        updated_unix_ms: unix_ms_now(),
    }
}

/// The last gate a temperature passes before it is published. Anything a GPU
/// sensor cannot produce is dropped to `None` rather than written out, so a
/// broken reading reaches the panel as "no sensor" instead of as a number.
fn sensor_temperature(temp_c: Option<f32>) -> Option<f32> {
    temp_c.filter(|c| c.is_finite() && *c > 0.0 && *c < 120.0)
}

/// The last gate a board-power reading passes before it is published, and the
/// same gate `MiningRuntimeState` applies before it stores one.
///
/// The window is deliberately loose at the top. This is a RIG total, the sum
/// over every card, and the tight per-board window (a graphics board draws under
/// 1000 W) has already been applied at the sensor by `gpu_temp_adl`. A ceiling
/// here that assumed one card would throw away the correct answer on a four-card
/// rig. What is left to reject is the shapes that are not measurements at all:
/// not finite, not positive, or a number no electrical installation could feed.
pub(crate) fn sensor_board_power_w(watts: Option<f32>) -> Option<f32> {
    watts.filter(|w| w.is_finite() && *w > 0.0 && *w < 100_000.0)
}

pub fn write_mining_stats(path: &str, stats: &MiningStatsSnapshot) {
    if path.is_empty() {
        return;
    }
    // This runs on every stats update, so surface failures WITHOUT spamming: log
    // the first error of a failing streak and stay quiet until it recovers. A
    // silently unwritten stats file is why a panel would show a frozen miner.
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    static WARNED: AtomicBool = AtomicBool::new(false);
    let result = serde_json::to_vec_pretty(stats)
        .map_err(|e| format!("serialize: {e}"))
        .and_then(|json| {
            atomic_write_private(Path::new(path), &json).map_err(|e| format!("write {path}: {e}"))
        });
    match result {
        Ok(()) => WARNED.store(false, Relaxed),
        Err(e) => {
            if !WARNED.swap(true, Relaxed) {
                wlogerr!("[stats] cannot update stats file ({e}); suppressing until it recovers");
            }
        }
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

    fn hac_stats(gpu_temp_c: Option<f32>) -> MiningStatsSnapshot {
        hac_stats_with_power(gpu_temp_c, None, 2)
    }

    fn hac_stats_with_power(
        gpu_temp_c: Option<f32>,
        gpu_board_power_w: Option<f32>,
        active_cpu: u32,
    ) -> MiningStatsSnapshot {
        build_mining_stats(
            1_000_000.0,
            0.5,
            0.01,
            &efficiency(),
            "amd_profit",
            active_cpu,
            765_432,
            false,
            1_536,
            0,
            0,
            1_536,
            900_000.0,
            100_000.0,
            gpu_temp_c,
            gpu_board_power_w,
        )
    }

    #[test]
    fn a_machine_with_no_sensor_publishes_no_temperature_rather_than_zero() {
        // A gauge reading 0C looks exactly like a working sensor on a cold
        // card, so the absence has to survive all the way to the JSON.
        let stats = hac_stats(None);
        assert_eq!(stats.gpu_temp_c, None);
        let json = serde_json::to_string(&stats).unwrap();
        let read: MiningStatsSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(read.gpu_temp_c, None);
    }

    #[test]
    fn a_measured_temperature_is_published_and_survives_the_json() {
        let stats = hac_stats(Some(67.5));
        assert_eq!(stats.gpu_temp_c, Some(67.5));
        let json = serde_json::to_string(&stats).unwrap();
        let read: MiningStatsSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(read.gpu_temp_c, Some(67.5));
    }

    #[test]
    fn a_reading_no_gpu_sensor_could_produce_is_dropped_not_published() {
        for impossible in [0.0, -40.0, 500.0, f32::NAN, f32::INFINITY] {
            assert_eq!(
                hac_stats(Some(impossible)).gpu_temp_c,
                None,
                "{impossible} is not a GPU temperature"
            );
        }
    }

    #[test]
    fn the_worker_carries_a_recorded_power_all_the_way_into_the_stats_file() {
        // The one link the hardware test cannot reach: what the monitor recorded
        // on the runtime has to arrive in the JSON the panel actually reads.
        let runtime = MiningRuntimeState::new(48, 0);
        runtime.record_gpu_board_power_w(291.0);
        let path = std::env::temp_dir().join(format!(
            "hacash-stats-power-{}-{}.json",
            std::process::id(),
            unix_ms_now()
        ));
        let agg = BatchAggregate {
            hashrate: 18_830_000.0,
            hac_per_day: 0.5,
            network_pct: 0.01,
            height: 768_000,
            gpu_hashrate: 18_830_000.0,
            cpu_hashrate: 0.0,
            paused: false,
        };
        emit_from_batch_aggregate(
            &agg,
            &efficiency(),
            "amd_profit",
            0,
            48,
            &runtime,
            "hac",
            0,
            "",
            path.to_str().unwrap(),
        );
        let saved: MiningStatsSnapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.gpu_board_power_w, Some(291.0));
        assert!(saved.watts_measured);
        assert!((saved.watts - 291.0).abs() < 0.000_001);
        // The real measured pair from the RX 9070 XT sweep: 18.83 MH/s at 291 W
        // is 64.7 kH/J. The 246 W estimate would have claimed 76.5.
        assert!((saved.kh_per_j - 64.71).abs() < 0.01, "{}", saved.kh_per_j);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn with_no_power_sensor_the_watts_are_the_configured_estimate_and_say_so() {
        // gpu_watts = 300 in the ini, amd_profit factor, plus 2 CPU threads at
        // 8 W. Whatever that arithmetic comes to, it is a guess, and the
        // snapshot has to admit it.
        let stats = hac_stats(Some(60.0));
        assert_eq!(stats.gpu_board_power_w, None);
        assert!(!stats.watts_measured);
        let estimate = efficiency().estimate_gpu_watts("amd_profit") + 2.0 * 8.0;
        assert!((stats.watts - estimate).abs() < 0.000_001);
    }

    #[test]
    fn a_measured_board_draw_replaces_the_estimate_everywhere_it_was_used() {
        // The card really measured 256 W. The 300 W ini guess must not survive
        // anywhere downstream: not in watts, not in kH/J, not in the daily cost.
        let measured = hac_stats_with_power(Some(60.0), Some(256.0), 0);
        let guessed = hac_stats_with_power(Some(60.0), None, 0);
        assert_eq!(measured.gpu_board_power_w, Some(256.0));
        assert!((measured.watts - 256.0).abs() < 0.000_001);
        assert!(measured.watts_measured);
        assert!(!guessed.watts_measured);

        // The guess on this config is 300 W of board times the 0.82 profit
        // factor, which is 246 W: it UNDERSTATES the card by 10 W. So the
        // measurement makes the rig look worse, not better, and every derived
        // figure has to move that way. Publishing a flattering guess is exactly
        // the failure being removed here.
        assert!(
            (guessed.watts - 246.0).abs() < 0.000_001,
            "{}",
            guessed.watts
        );
        assert!(measured.watts > guessed.watts);
        assert!(measured.kh_per_j < guessed.kh_per_j);
        assert!(measured.daily_cost_eur > guessed.daily_cost_eur);
        assert!((measured.kh_per_j - 1_000_000.0 / 256.0 / 1000.0).abs() < 0.000_001);
        // 256 W for 24 h at 0.25 EUR/kWh.
        assert!((measured.daily_cost_eur - 256.0 * 24.0 / 1000.0 * 0.25).abs() < 0.000_001);
        assert!(
            (measured.daily_net_eur - (measured.daily_revenue_eur - measured.daily_cost_eur)).abs()
                < 0.000_001
        );
    }

    #[test]
    fn a_measured_gpu_plus_an_estimated_cpu_is_not_called_a_measurement() {
        // Two CPU assist threads at a per-thread guess. The GPU figure is still
        // published, because it is real, but the TOTAL is part guess and the
        // panel must not be allowed to label it "measured".
        let stats = hac_stats_with_power(Some(60.0), Some(256.0), 2);
        assert_eq!(stats.gpu_board_power_w, Some(256.0));
        assert!(!stats.watts_measured);
        assert!((stats.watts - (256.0 + 16.0)).abs() < 0.000_001);
    }

    #[test]
    fn a_reading_no_board_sensor_could_produce_is_dropped_not_published() {
        // Zero is the one that matters: it is what an unsupported ADL slot
        // holds, and published as watts it turns into free electricity, an
        // infinite kH/J and a rig that can never be paused for cost.
        for impossible in [0.0, -1.0, f32::NAN, f32::INFINITY, 100_000.0] {
            let stats = hac_stats_with_power(Some(60.0), Some(impossible), 0);
            assert_eq!(
                stats.gpu_board_power_w, None,
                "{impossible} is not a board draw"
            );
            assert!(!stats.watts_measured);
            assert!(
                stats.watts > 0.0,
                "a rejected reading must fall back to the estimate, not to zero"
            );
        }
    }

    #[test]
    fn a_multi_card_rig_total_is_above_one_board_and_still_accepted() {
        // The per-board window (under 1000 W) lives at the sensor. What arrives
        // here is a SUM over cards, so a four-card rig at 1024 W is a correct
        // reading and rejecting it would be the bug.
        let stats = hac_stats_with_power(Some(60.0), Some(1_024.0), 0);
        assert_eq!(stats.gpu_board_power_w, Some(1_024.0));
        assert!((stats.watts - 1_024.0).abs() < 0.000_001);
    }

    #[test]
    fn a_measured_power_survives_the_json_and_an_absent_one_stays_absent() {
        for measured in [None, Some(256.0f32)] {
            let stats = hac_stats_with_power(Some(60.0), measured, 0);
            let json = serde_json::to_string(&stats).unwrap();
            let read: MiningStatsSnapshot = serde_json::from_str(&json).unwrap();
            assert_eq!(read.gpu_board_power_w, measured);
            assert_eq!(read.watts_measured, stats.watts_measured);
        }
    }

    #[test]
    fn a_stats_file_written_before_the_sensor_existed_still_reads_back() {
        // Older workers wrote no temperature field at all. That file must load
        // as "no reading", not fail the whole snapshot and freeze the panel.
        let legacy = r#"{"status":"mining","hashrate_hps":1.0,"hashrate_display":"1H/s",
            "watts":10.0,"kh_per_j":0.1,"hac_per_day":0.0,"network_pct":0.0,
            "daily_cost_eur":0.0,"daily_revenue_eur":0.0,"daily_net_eur":0.0,"height":7,
            "gpu_profile":"amd_profit","gpu_hashrate_hps":1.0,"cpu_hashrate_hps":0.0,
            "gpu_hashrate_display":"1H/s","active_cpu_threads":0,"paused_unprofitable":false,
            "mining_kind":"hac","diamond_number":0,"diamond_best":"","updated_unix_ms":1}"#;
        let read: MiningStatsSnapshot = serde_json::from_str(legacy).unwrap();
        assert_eq!(read.gpu_temp_c, None);
        assert_eq!(read.height, 7);
        // The same file predates the power sensor too, and an installed worker
        // still writes it. It must load as "not measured" rather than as a rig
        // drawing zero watts.
        assert_eq!(read.gpu_board_power_w, None);
        assert!(!read.watts_measured);
    }

    #[test]
    fn a_healthy_rig_publishes_the_full_work_group_count_under_the_oom_field() {
        // The operator's rig: 48 configured, 48 allowed, nothing clamped. This
        // is the shape that made the panel draw a red ring, so it is written
        // down here as the normal case it actually is.
        let stats = build_mining_stats(
            1_000_000.0,
            0.5,
            0.01,
            &efficiency(),
            "amd_profit",
            2,
            768_566,
            false,
            48,
            48,
            0,
            48,
            900_000.0,
            100_000.0,
            None,
            None,
        );
        assert_eq!(stats.oom_allowed_work_groups, stats.configured_work_groups);
        assert_eq!(stats.thermal_cap_work_groups, 0);
    }

    #[test]
    fn a_stats_file_written_under_the_old_oom_key_still_loads() {
        // The field was renamed because its old name read as a loss. A worker
        // already installed on disk still writes the old key, and its snapshot
        // must not come back as zero work groups.
        let legacy = r#"{"status":"mining","hashrate_hps":1.0,"hashrate_display":"1H/s",
            "watts":10.0,"kh_per_j":0.1,"hac_per_day":0.0,"network_pct":0.0,
            "daily_cost_eur":0.0,"daily_revenue_eur":0.0,"daily_net_eur":0.0,"height":7,
            "gpu_profile":"amd_profit","configured_work_groups":48,"oom_work_groups":48,
            "thermal_cap_work_groups":0,"effective_work_groups":48,
            "gpu_hashrate_hps":1.0,"cpu_hashrate_hps":0.0,
            "gpu_hashrate_display":"1H/s","active_cpu_threads":0,"paused_unprofitable":false,
            "mining_kind":"hac","diamond_number":0,"diamond_best":"","updated_unix_ms":1}"#;
        let read: MiningStatsSnapshot = serde_json::from_str(legacy).unwrap();
        assert_eq!(read.oom_allowed_work_groups, 48);
        assert_eq!(read.configured_work_groups, 48);
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
        assert!(stats.gpu_temp_c.is_none());
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

pub(crate) fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
