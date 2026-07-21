#![cfg(feature = "ocl")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use app::efficiency::MiningStatsSnapshot;
use app::mining_runtime::MiningRuntimeState;
use app::poworker::{PoWorkConf, poworker_with_stop};
use field::Hash;
use testkit::sim::miner_api::{MinerApiSim, MinerPendingStuff};

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn configured_opencl_poworker(rpcaddr: String) -> PoWorkConf {
    let mut cnf = PoWorkConf::test_defaults(rpcaddr, 0, u32::MAX);
    cnf.useopencl = true;
    cnf.usecuda = false;
    cnf.cpu_assist = false;
    cnf.workgroups = env_u32("HACASH_OPENCL_WORK_GROUPS", 48);
    cnf.localsize = 256;
    cnf.unitsize = env_u32("HACASH_OPENCL_UNIT_SIZE", 48);
    cnf.opencldir = std::env::var("HACASH_OPENCL_DIR").unwrap_or_else(|_| {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("app crate must have a workspace parent")
            .join("x16rs")
            .join("opencl")
            .to_string_lossy()
            .into_owned()
    });
    cnf.platformid = env_u32("HACASH_OPENCL_PLATFORM_ID", 0);
    cnf.deviceids = std::env::var("HACASH_OPENCL_DEVICE_IDS").unwrap_or_else(|_| "0".to_string());
    cnf.gpu_profile = "amd_profit".to_string();
    cnf.gpu_slug = "gfx1201".to_string();
    cnf.efficiency.dynamic_supervene = false;
    cnf.efficiency.benchmark_seconds = 0;
    cnf.efficiency.max_temp_c = 0;
    cnf.runtime = MiningRuntimeState::new(cnf.workgroups, 0);
    cnf
}

#[test]
#[ignore = "requires a physical OpenCL GPU; set HACASH_RUN_OPENCL_INTEGRATION=1"]
fn poworker_opencl_production_path_on_real_gpu() {
    assert_eq!(
        std::env::var_os("HACASH_RUN_OPENCL_INTEGRATION").as_deref(),
        Some(std::ffi::OsStr::new("1")),
        "set HACASH_RUN_OPENCL_INTEGRATION=1 before running this ignored GPU test"
    );

    let soak_seconds = env_u32("HACASH_OPENCL_SOAK_SECONDS", 0) as u64;
    assert!(
        soak_seconds == 0 || soak_seconds >= 5,
        "a stability soak must run for at least five seconds"
    );
    let mut pending = MinerPendingStuff::easy_for_test(2);
    if soak_seconds > 0 {
        pending.target_hash = Hash::from([0u8; 32]);
    }
    let sim = MinerApiSim::start(pending);
    let stop = Arc::new(AtomicBool::new(false));

    let mut cnf = configured_opencl_poworker(sim.rpcaddr().to_string());
    let expected_workgroups = cnf.workgroups;
    let runtime = cnf.runtime.clone();
    let stats_path = if soak_seconds > 0 {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hacash-opencl-soak-{}-{suffix}.json",
            std::process::id()
        ));
        cnf.efficiency.stats_file = path.to_string_lossy().into_owned();
        Some(path)
    } else {
        None
    };

    let worker_stop = stop.clone();
    let worker = thread::spawn(move || poworker_with_stop(cnf, Some(worker_stop)));

    let mut snapshots: Vec<(Instant, MiningStatsSnapshot)> = Vec::new();
    let mut parse_failures = 0u32;
    let submitted = if let Some(path) = stats_path.as_ref() {
        let startup_deadline = Instant::now() + Duration::from_secs(45);
        let mut soak_deadline = None;
        let mut last_update = 0u64;
        loop {
            let now = Instant::now();
            if soak_deadline.is_some_and(|deadline| now >= deadline) {
                break;
            }
            assert!(
                soak_deadline.is_some() || now < startup_deadline,
                "OpenCL worker produced no stats within 45 seconds"
            );

            if let Ok(data) = std::fs::read_to_string(path) {
                match serde_json::from_str::<MiningStatsSnapshot>(&data) {
                    Ok(stats) if stats.updated_unix_ms != last_update => {
                        let observed_at = Instant::now();
                        last_update = stats.updated_unix_ms;
                        soak_deadline
                            .get_or_insert(observed_at + Duration::from_secs(soak_seconds.max(1)));
                        snapshots.push((observed_at, stats));
                    }
                    Ok(_) => {}
                    Err(_) => parse_failures += 1,
                }
            }
            thread::sleep(Duration::from_millis(200));
        }
        false
    } else {
        sim.wait_for_submit(1, Duration::from_secs(45))
    };
    let sampling_finished = Instant::now();
    stop.store(true, Ordering::Release);
    worker.join().expect("join OpenCL poworker");

    // The controller owns detached GPU/result threads in production. Wait for their explicit
    // runtime acknowledgement instead of guessing how long the final GPU batch may take.
    let shutdown_deadline = Instant::now() + Duration::from_secs(30);
    while runtime.active_mining_threads() > 0 && Instant::now() < shutdown_deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        runtime.active_mining_threads(),
        0,
        "OpenCL mining threads did not acknowledge shutdown within 30 seconds"
    );
    let last_submit = sim.last_submit();
    drop(sim);

    if let Some(path) = stats_path {
        let _ = std::fs::remove_file(path);
    }

    assert_eq!(
        runtime.gpu_errors.load(Ordering::Acquire),
        0,
        "OpenCL production path reported GPU errors"
    );

    if soak_seconds == 0 {
        assert!(
            submitted,
            "OpenCL poworker did not submit to the simulated miner API"
        );
        assert_eq!(last_submit.get("height"), Some(&"2".to_string()));
        return;
    }

    assert!(
        last_submit.is_empty(),
        "impossible target was unexpectedly submitted"
    );
    assert_eq!(parse_failures, 0, "observed a partial stats JSON write");
    assert!(
        snapshots.len() >= 3,
        "expected at least 3 distinct stats samples, got {}",
        snapshots.len()
    );

    let (first_observed, _) = snapshots.first().unwrap();
    let (last_observed, last) = snapshots.last().unwrap();
    assert!(
        sampling_finished.saturating_duration_since(*last_observed) <= Duration::from_secs(2),
        "stats stopped more than two seconds before the soak ended"
    );
    let required_span = Duration::from_secs(soak_seconds).saturating_sub(Duration::from_secs(2));
    assert!(
        last_observed.saturating_duration_since(*first_observed) >= required_span,
        "stats did not remain live for the requested soak duration"
    );
    assert!(last.gpu_hashrate_hps > 1_000_000.0, "{last:?}");
    assert!(last.gpu_hashrate_display.contains("MH/s"), "{last:?}");
    assert_eq!(last.configured_work_groups, expected_workgroups);
    assert_eq!(last.effective_work_groups, expected_workgroups);

    for pair in snapshots.windows(2) {
        assert!(
            pair[1].0.saturating_duration_since(pair[0].0) <= Duration::from_secs(2),
            "stats observation gap exceeded two seconds"
        );
        assert!(
            pair[1].1.updated_unix_ms > pair[0].1.updated_unix_ms,
            "stats timestamps did not advance monotonically"
        );
    }

    let recent = &snapshots[snapshots.len().saturating_sub(8)..];
    assert!(recent.len() >= 3);
    for (_, stats) in recent {
        assert!(
            stats.gpu_hashrate_hps.is_finite() && stats.gpu_hashrate_hps > 1_000_000.0,
            "recent GPU rate dropped out: {stats:?}"
        );
        assert!(stats.gpu_hashrate_display.contains("MH/s"), "{stats:?}");
        assert_eq!(stats.configured_work_groups, expected_workgroups);
        assert_eq!(stats.effective_work_groups, expected_workgroups);
    }
    let min_rate = recent
        .iter()
        .map(|(_, stats)| stats.gpu_hashrate_hps)
        .fold(f64::INFINITY, f64::min);
    let max_rate = recent
        .iter()
        .map(|(_, stats)| stats.gpu_hashrate_hps)
        .fold(0.0, f64::max);
    assert!(
        min_rate >= max_rate * 0.5,
        "recent GPU rate was unstable: min={min_rate} max={max_rate}"
    );
}
