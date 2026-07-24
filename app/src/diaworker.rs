use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::*};
use std::sync::{RwLock, mpsc};

use std::thread::*;
use std::time::*;

use reqwest::blocking::Client as HttpClient;
use serde_json::Value as JV;

use crate::efficiency::*;
// Same panic firewall the block miner uses: one result thread owns every
// submission, so a panic there would silently end all payouts.
use crate::mining_guard::guard_mining_iteration;

use basis::difficulty::*;
use field::*;
use mint::action::*;
use mint::genesis::*;
use sys::*;

use crate::hash_util::diamond_more_power;

#[cfg(feature = "ocl")]
use crate::gpu_oom::GpuBatchError;
#[cfg(feature = "ocl")]
use crate::opencl_gpu::{OpenclGpuHandle, initialize_opencl, opencl_snapshot_from_resource};
#[cfg(feature = "ocl")]
#[path = "opencl_dia.rs"]
mod opencl_dia;
#[cfg(feature = "ocl")]
use opencl_dia::do_diamond_group_mining_opencl;

/*************************************/

#[derive(Clone)]
pub struct DiaWorkConf {
    pub rpcaddr: String,
    /// Optional fullnode API token (`X-Api-Token`) when server requires auth.
    pub api_token: String,
    pub supervene: u32, // cpu core
    pub bidaddr: Address,
    pub rewardaddr: Address,
    pub useopencl: bool,   // use opencl miner
    pub workgroups: u32,   // opencl work groups
    pub localsize: u32,    // opencl work units per work group
    pub unitsize: u32,     // opencl hashes per work unit
    pub opencldir: String, // opencl source dir
    pub debug: u32,        // enable debug mode
    pub platformid: u32,   // opencl platform id
    pub deviceids: String, // opencl device id list
    pub cpu_assist: bool,
    pub gpu_profile: String,
    pub gpu_slug: String,
    pub efficiency: EfficiencyConf,
    pub runtime: Arc<MiningRuntimeState>,
}

impl DiaWorkConf {
    pub fn new(ini: &IniObj) -> DiaWorkConf {
        let sec = &ini_section(ini, "default"); // default = root
        let efficiency = EfficiencyConf::from_ini(ini);
        let configured_supervene = (ini_must_u64(sec, "supervene", 2) as u32).max(1);
        let active = efficiency.initial_active_supervene(configured_supervene);
        let runtime = MiningRuntimeState::new(0, active);
        // HACD is officially CPU/full-node mining. Legacy GPU keys are ignored
        // so a stale or hand-edited config cannot activate the OpenCL path. Warn
        // loudly if the config still carries GPU keys, so it is clear they do
        // nothing here (rather than silently forcing CPU).
        let wants_gpu = ["useopencl", "usecuda"].iter().any(|k| {
            matches!(
                ini_must(sec, k, "").trim().to_lowercase().as_str(),
                "true" | "1" | "yes"
            )
        }) || ini_must_u64(sec, "workgroups", 0) > 0;
        if wants_gpu {
            println!(
                "[diamond] NOTE: HACD (diamond) mining is CPU / full-node only; the GPU keys \
                 in this config (useopencl / usecuda / workgroups) are ignored."
            );
        }
        DiaWorkConf {
            rpcaddr: ini_must(sec, "connect", "127.0.0.1:8081"),
            api_token: ini_must(sec, "api_token", "").trim().to_string(),
            supervene: configured_supervene,
            bidaddr: Address::default(),
            rewardaddr: Address::default(),
            useopencl: false,
            workgroups: 0,
            localsize: 256,
            unitsize: 0,
            opencldir: String::new(),
            debug: 0,
            platformid: 0,
            deviceids: String::new(),
            cpu_assist: false,
            gpu_profile: String::new(),
            gpu_slug: "none".to_string(),
            efficiency,
            runtime,
        }
    }
}

#[cfg(test)]
mod config_tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn legacy_hacd_gpu_config_is_forced_to_cpu_only() {
        let mut ini = IniObj::new();
        let mut default = HashMap::new();
        default.insert("supervene".to_string(), Some("0".to_string()));
        ini.insert("default".to_string(), default);
        let mut gpu = HashMap::new();
        gpu.insert("use_opencl".to_string(), Some("true".to_string()));
        gpu.insert("cpu_assist".to_string(), Some("true".to_string()));
        gpu.insert("work_groups".to_string(), Some("4096".to_string()));
        gpu.insert("gpu_profile".to_string(), Some("amd_max".to_string()));
        ini.insert("gpu".to_string(), gpu);

        let config = DiaWorkConf::new(&ini);
        assert_eq!(config.supervene, 1);
        assert!(!config.useopencl);
        assert!(!config.cpu_assist);
        assert_eq!(config.workgroups, 0);
        assert_eq!(config.unitsize, 0);
        assert_eq!(config.gpu_slug, "none");
        assert!(config.gpu_profile.is_empty());
    }
}

/*************************************/

const HASH_WIDTH: usize = 32;
// Length of a diamond hash string (x16rs DMD_M): 10 leading '0' chars followed by
// the 6-char diamond name. This is a fixed mainnet consensus constant.
pub(crate) const DIAMOND_HASH_LEN: usize = 16;
const MINING_INTERVAL: f64 = 3.0; // 3 secs
/// Bounded result channel: an unbounded queue grows without limit whenever the
/// drain thread stalls. Under backpressure a statistics-only batch may be
/// dropped, but a batch carrying a mined diamond is real money and always waits.
const RESULT_CHANNEL_CAPACITY: usize = 1024;
/// Mined diamonds queued for the dedicated submit thread. A diamond is rare, so
/// this only has to absorb a burst while one submission is in flight.
const SUBMIT_QUEUE_CAPACITY: usize = 64;

// current mining diamond number
static MINING_DIAMOND_NUM: AtomicU32 = AtomicU32::new(0);

use std::sync::LazyLock;
static HTTP_CLIENT: LazyLock<HttpClient> = LazyLock::new(|| {
    crate::rpc_http::build_client()
        .unwrap_or_else(|e| panic!("cannot create bounded RPC client: {e}"))
});
static MINING_DIAMOND_STUFF: LazyLock<RwLock<Hash>> = LazyLock::new(|| RwLock::default());

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub(crate) struct DiamondMiningResult {
    number: u32,
    nonce_start: u64,
    nonce_space: u64,
    u64_nonce: u64,
    msg_nonce: Vec<u8>,
    dia_str: [u8; DIAMOND_HASH_LEN],
    is_success: Option<DiamondMint>,
    use_secs: f64,
    is_gpu: bool,
    gpu_batch_ok: bool,
}

fn should_stop(stop_flag: &Option<Arc<AtomicBool>>) -> bool {
    stop_flag.as_ref().map(|f| f.load(Relaxed)).unwrap_or(false)
}

/// Spawn a diamond mining thread that is registered with the runtime, exactly
/// like the block miner registers its own threads, so a shutdown supervisor can
/// wait for every HACD thread with `MiningRuntimeState::active_mining_threads`.
/// The guard is taken BEFORE the thread starts, so the count can never read zero
/// while a thread is still on its way up, and it is released by Drop, so a thread
/// that returns (or unwinds) always acknowledges instead of hanging the wait.
fn spawn_tracked_diamond_thread<F>(runtime: &Arc<MiningRuntimeState>, body: F)
where
    F: FnOnce() + Send + 'static,
{
    let thread_guard = runtime.track_mining_thread();
    spawn(move || {
        let _thread_guard = thread_guard;
        body();
    });
}

/// Hand a batch result to the drain thread. Returns false only when the drain
/// side is gone, so the worker knows to exit. Under backpressure a
/// statistics-only result is dropped with a log, but a result carrying a mined
/// diamond is a payout and waits for space instead.
fn send_diamond_result(
    result_ch_tx: &mpsc::SyncSender<DiamondMiningResult>,
    res: DiamondMiningResult,
) -> bool {
    match result_ch_tx.try_send(res) {
        Ok(()) => true,
        Err(mpsc::TrySendError::Full(res)) => {
            if res.is_success.is_some() {
                return result_ch_tx.send(res).is_ok();
            }
            eprintln!(
                "[Mining] Diamond result queue full, dropped a statistics-only batch at number {}.",
                res.number
            );
            true
        }
        Err(mpsc::TrySendError::Disconnected(_)) => false,
    }
}

/// Queue a mined diamond for the submit thread. If the queue is full or the
/// thread is gone we submit inline rather than drop it: a dropped diamond is
/// lost money.
fn queue_diamond_mining_success(
    cnf: &DiaWorkConf,
    submit_tx: &mpsc::SyncSender<DiamondMint>,
    success: DiamondMint,
) {
    match submit_tx.try_send(success) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(success)) => {
            eprintln!(
                "[Mining] Diamond submit queue full, submitting number {} inline.",
                *success.d.number
            );
            push_diamond_mining_success(cnf, success);
        }
        Err(mpsc::TrySendError::Disconnected(success)) => {
            push_diamond_mining_success(cnf, success);
        }
    }
}

/*
* Diamond worker
*/
pub fn diaworker() {
    diaworker_with_stop(None)
}

pub fn diaworker_with_stop(stop_flag: Option<Arc<AtomicBool>>) {
    let cnfp = "./diaworker.config.ini".to_string();
    let inicnf = sys::load_config(cnfp.clone());
    let mut cnf = DiaWorkConf::new(&inicnf);
    if cnf.efficiency.benchmark_seconds > 0 {
        run_diamond_mining_benchmark(&cnf, &cnfp);
        return;
    }

    // test start
    // cnf.supervene = 1;
    // test end

    let (res_tx, res_rx) = mpsc::sync_channel(RESULT_CHANNEL_CAPACITY);

    // init
    load_init(&mut cnf);

    // Initialize OpenCL
    #[cfg(feature = "ocl")]
    let (scan, amd_icd_count, opencl_resources) = if cnf.useopencl {
        let scan = crate::opencl_diag::scan_opencl();
        let amd_icd_count = crate::opencl_diag::count_amd_platforms(&scan.platforms);
        let resources = initialize_opencl(
            true,
            &cnf.opencldir,
            &cnf.platformid,
            &cnf.deviceids,
            &cnf.workgroups,
            &cnf.localsize,
            &cnf.unitsize,
            Some(&scan),
            false,
        );
        (scan, amd_icd_count, resources)
    } else {
        (
            crate::opencl_diag::OpenClScan {
                platforms: Vec::new(),
                warnings: Vec::new(),
                recommended: None,
            },
            0usize,
            Vec::new(),
        )
    };
    #[cfg(feature = "ocl")]
    if cnf.useopencl && opencl_resources.is_empty() {
        eprintln!(
            "[Fatal] OpenCL was requested but no usable GPU backend initialized; stopping HACD worker."
        );
        return;
    }

    // Calculate device/cpu quantity
    #[cfg(feature = "ocl")]
    let vene: u32 = if cnf.useopencl {
        let gpu = opencl_resources.len() as u32;
        if cnf.cpu_assist {
            gpu.saturating_add(cnf.efficiency.spawn_supervene(cnf.supervene))
        } else {
            gpu
        }
    } else {
        cnf.efficiency.clamp_supervene(cnf.supervene)
    };
    #[cfg(not(feature = "ocl"))]
    let vene: u32 = cnf.supervene;

    // Submitting a mined diamond is a blocking HTTP round-trip (with retries), so
    // it runs on a dedicated thread: doing it inline would stall the 77ms result
    // drain and back the result channel up.
    let (submit_tx, submit_rx) = mpsc::sync_channel::<DiamondMint>(SUBMIT_QUEUE_CAPACITY);
    let cnf_submit = cnf.clone();
    spawn_tracked_diamond_thread(&cnf.runtime, move || {
        while let Ok(success) = submit_rx.recv() {
            guard_mining_iteration("diamond submit thread", || {
                push_diamond_mining_success(&cnf_submit, success);
            });
        }
    });

    // deal results
    let cnf1 = cnf.clone();
    let stop_flag_res = stop_flag.clone();
    spawn_tracked_diamond_thread(&cnf.runtime, move || {
        // submit_tx lives in this thread: when the drain loop returns on shutdown
        // it drops, which is what tells the submit thread to finish and exit.
        let mut most_dia_str = [b'W'; DIAMOND_HASH_LEN];
        let mut rstx = res_rx;
        loop {
            if should_stop(&stop_flag_res) {
                return;
            }
            // A panic here must never end the thread: this is the only path that
            // submits mined diamonds, so losing it means silent total payout loss
            // while the miner still looks like it is running.
            guard_mining_iteration("diamond result thread", || {
                deal_diamond_mining_results(&cnf1, &mut most_dia_str, &mut rstx, vene, &submit_tx);
            });
            delay_continue_ms!(77);
        }
    });

    // start worker
    if cnf.useopencl {
        // opencl is enabled
        #[cfg(feature = "ocl")]
        {
            // Initialize OpenCL
            println!(
                "\n[Start] Create GPU diamond miner worker #{}.",
                opencl_resources.len()
            );
            for (thrid, resource) in opencl_resources.into_iter().enumerate() {
                let vram = resource.vram_bytes;
                let arch = resource.arch_slug.clone();
                let gpu_snapshot = opencl_snapshot_from_resource(
                    &resource,
                    true,
                    &cnf.opencldir,
                    cnf.localsize,
                    cnf.unitsize,
                    amd_icd_count,
                );
                let gpu = OpenclGpuHandle::new(resource, gpu_snapshot, scan.clone());
                gpu.configure_oom_floor(vram, cnf.localsize, cnf.unitsize, cnf.workgroups, &arch);
                let cnf2 = cnf.clone();
                let rstx: mpsc::SyncSender<DiamondMiningResult> = res_tx.clone();
                let stop_flag_worker = stop_flag.clone();
                spawn_tracked_diamond_thread(&cnf.runtime, move || {
                    loop {
                        if should_stop(&stop_flag_worker) {
                            return;
                        }
                        guard_mining_iteration("diamond GPU mining worker", || {
                            run_diamond_worker_thread_opencl(
                                &cnf2,
                                thrid,
                                rstx.clone(),
                                gpu.clone(),
                                &stop_flag_worker,
                            );
                        });
                        delay_continue_ms!(9);
                    }
                });
            }
        }
        #[cfg(not(feature = "ocl"))]
        {
            println!(
                "[Warning] use_opencl=true but app built without feature 'ocl'; fallback to CPU mining."
            );
            let thrnum = cnf.efficiency.clamp_supervene(cnf.supervene) as usize;
            println!("\n[Start] Create #{} diamond miner worker thread.", thrnum);
            for thrid in 0..thrnum {
                let cnf2 = cnf.clone();
                let rstx = res_tx.clone();
                let stop_flag_worker = stop_flag.clone();
                spawn_tracked_diamond_thread(&cnf.runtime, move || {
                    loop {
                        if should_stop(&stop_flag_worker) {
                            return;
                        }
                        guard_mining_iteration("diamond mining worker", || {
                            run_diamond_worker_thread(&cnf2, thrid, rstx.clone(), &stop_flag_worker);
                        });
                        delay_continue_ms!(9);
                    }
                });
            }
        }

        if cnf.cpu_assist && cnf.supervene > 0 {
            #[cfg(feature = "ocl")]
            {
                let thrnum = cnf.efficiency.spawn_supervene(cnf.supervene) as usize;
                println!(
                    "\n[Start] Create #{} Ryzen CPU assist threads for diamonds (hybrid).",
                    thrnum
                );
                for thrid in 0..thrnum {
                    let cnf2 = cnf.clone();
                    let rstx = res_tx.clone();
                    let stop_flag_worker = stop_flag.clone();
                    spawn_tracked_diamond_thread(&cnf.runtime, move || {
                        loop {
                            if should_stop(&stop_flag_worker) {
                                return;
                            }
                            guard_mining_iteration("diamond CPU assist worker", || {
                                run_diamond_worker_thread(
                                    &cnf2,
                                    thrid,
                                    rstx.clone(),
                                    &stop_flag_worker,
                                );
                            });
                            delay_continue_ms!(9);
                        }
                    });
                }
            }
        }
    } else {
        let thrnum = cnf.efficiency.clamp_supervene(cnf.supervene) as usize;
        println!("\n[Start] Create #{} diamond miner worker thread.", thrnum);
        for thrid in 0..thrnum {
            let cnf2 = cnf.clone();
            let rstx = res_tx.clone();
            let stop_flag_worker = stop_flag.clone();
            spawn_tracked_diamond_thread(&cnf.runtime, move || {
                loop {
                    if should_stop(&stop_flag_worker) {
                        return;
                    }
                    guard_mining_iteration("diamond mining worker", || {
                        run_diamond_worker_thread(&cnf2, thrid, rstx.clone(), &stop_flag_worker);
                    });
                    delay_continue_ms!(9);
                }
            });
        }
    }

    // pull loop
    loop {
        if should_stop(&stop_flag) {
            return;
        }
        if !is_within_idle_schedule(cnf.efficiency.idle_start_hour, cnf.efficiency.idle_end_hour) {
            delay_continue!(5);
        }
        if cnf.runtime.paused_unprofitable.load(Relaxed) {
            delay_continue!(3);
        }
        // HACD is CPU-only; GPU temperature polling does not apply here.
        pull_and_push_diamond(&cnf);
        delay_continue!(MINING_INTERVAL as u64);
    }
}

fn deal_diamond_mining_results(
    cnf: &DiaWorkConf,
    most_dia_str: &mut [u8; DIAMOND_HASH_LEN],
    result_ch_rx: &mut mpsc::Receiver<DiamondMiningResult>,
    vene: u32,
    submit_tx: &mpsc::SyncSender<DiamondMint>,
) {
    let mut deal_number = 0u32;
    let mut most = DiamondMiningResult::default();
    most.dia_str = [b'w'; DIAMOND_HASH_LEN];
    let mut total_nonce_space = 0u64;
    let mut gpu_nonce_space = 0u64;
    let mut cpu_nonce_space = 0u64;
    let mut total_use_secs = 0.0;
    let mut recv_count = 0;
    while let Ok(res) = result_ch_rx.try_recv() {
        deal_number = res.number;
        total_nonce_space += res.nonce_space as u64;
        if res.is_gpu {
            gpu_nonce_space += res.nonce_space as u64;
        } else {
            cpu_nonce_space += res.nonce_space as u64;
        }
        total_use_secs += res.use_secs;
        if diamond_more_power(&res.dia_str, &most.dia_str) {
            most = res.clone();
        }
        // upload success
        if let Some(success) = &res.is_success {
            queue_diamond_mining_success(cnf, submit_tx, success.clone());
        }
        recv_count += 1;
        if recv_count >= vene as usize * 4 {
            break;
        } // prevent infinite loop
    }
    if recv_count == 0 {
        return;
    }
    // total most
    if diamond_more_power(&most.dia_str, most_dia_str) {
        *most_dia_str = most.dia_str.clone();
    }
    // print hashrate
    let diastr = String::from_utf8_lossy(&most.dia_str).into_owned();
    let most_diastr = String::from_utf8_lossy(most_dia_str).into_owned();
    // Aggregate hashrate = total nonces / wall-clock. The workers run in parallel,
    // so wall-clock ~= total_use_secs / (parallel workers). Dividing by recv_count
    // (batches drained) instead would multiply the rate by the number of batches
    // each worker sent per drain, wildly overcounting for sequential batches.
    let parallelism = (vene.max(1)) as f64;
    let wall_secs = total_use_secs / parallelism;
    let nonce_rates = if wall_secs.is_finite() && wall_secs > 0.0 {
        total_nonce_space as f64 / wall_secs
    } else {
        0.0
    };
    let active_cpu = cnf.runtime.active_cpu_assist.load(Relaxed);
    cnf.runtime
        .maybe_adjust_supervene(&cnf.efficiency, gpu_nonce_space, cpu_nonce_space);
    if should_pause_for_diamond_profit(&cnf.efficiency, &cnf.gpu_profile, active_cpu) {
        cnf.runtime.paused_unprofitable.store(true, Relaxed);
        println!(
            "\n[efficiency] HACD mining paused: daily power cost exceeds configured revenue target (hac_price)."
        );
    } else {
        cnf.runtime.paused_unprofitable.store(false, Relaxed);
    }
    let paused = cnf.runtime.paused_unprofitable.load(Relaxed);
    // HACD is strictly CPU-only, so there is no GPU power draw. Using the shared
    // estimate_gpu_watts("") here would print a phantom ~280 W (Unknown vendor)
    // for a CPU miner, so report CPU-only wattage instead.
    let gpu_w = 0.0;
    let watts = gpu_w + active_cpu as f64 * cnf.efficiency.cpu_watts_per_thread;
    let hashrate_show = rates_to_show(nonce_rates);
    flush!(
        "[{}] {} | {}W | {} | {} -> {}.        \r",
        deal_number,
        hashrate_show,
        watts as u32,
        diastr,
        most_diastr,
        cnf.gpu_profile
    );
    crate::mining_stats::emit_from_batch_aggregate(
        &crate::mining_stats::BatchAggregate {
            hashrate: nonce_rates,
            hac_per_day: 0.0,
            network_pct: 0.0,
            height: deal_number as u64,
            gpu_hashrate: 0.0,
            cpu_hashrate: nonce_rates,
            paused,
        },
        &cnf.efficiency,
        &cnf.gpu_profile,
        active_cpu,
        cnf.workgroups,
        &cnf.runtime,
        "hacd",
        deal_number,
        &most_diastr,
        &cnf.efficiency.stats_file,
    );

    // print next
    may_print_turn_to_nex_diamond_mining(deal_number, Some(most_dia_str));
}

fn may_print_turn_to_nex_diamond_mining(
    curr_number: u32,
    most_dia_str: Option<&mut [u8; DIAMOND_HASH_LEN]>,
) {
    let mining_number = MINING_DIAMOND_NUM.load(Acquire);
    if mining_number <= curr_number {
        return; // not turn
    }
    if let Some(most_dia_str) = most_dia_str {
        *most_dia_str = [b'W'; DIAMOND_HASH_LEN]; // reset
    }

    println!(
        "\n[{}] req next number {} to mining ... ",
        &ctshow()[5..],
        mining_number
    );
}

//
fn run_diamond_worker_thread(
    cnf: &DiaWorkConf,
    _thrid: usize,
    result_ch_tx: mpsc::SyncSender<DiamondMiningResult>,
    stop_flag: &Option<Arc<AtomicBool>>,
) {
    if mining_is_gated(&cnf.runtime, &cnf.efficiency) {
        delay_return_ms!(2000);
    }
    let cmdn = MINING_DIAMOND_NUM.load(Acquire);
    if cmdn == 0 {
        delay_return_ms!(99); // not yet
    }
    #[cfg(feature = "ocl")]
    if cnf.useopencl && cnf.cpu_assist {
        let active = cnf.runtime.active_cpu_assist.load(Relaxed);
        if (_thrid as u32) >= active {
            delay_return_ms!(400);
        }
    }

    let rwd_addr = cnf.rewardaddr.clone();

    let mut nonce_space: u64 = 15000;
    let current_mining_number: u32 = cmdn;
    let current_mining_block_hash: Hash = {
        MINING_DIAMOND_STUFF
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    };

    // start mining
    let mut custom_nonce = [0u8; HASH_WIDTH];
    if let Err(e) = getrandom::fill(&mut custom_nonce) {
        eprintln!("[Mining] Secure random nonce failed: {e}");
        return;
    }
    let custom_nonce = Hash::from(custom_nonce);
    // Note: All threads starting from nonce_start = 0 here is not a bug:
    // each thread/task has been assigned a random custom_nonce above,
    // so x16rs::mine_diamond input differs; even with the same nonce_start,
    // the actual search hash space is disjoint and no hashrate conflict occurs.
    let mut nonce_start = 0;

    loop {
        // This inner loop only ends when the diamond number turns over, which can
        // be many minutes away, so it has to observe the stop flag itself for a
        // shutdown supervisor to see this thread acknowledge in good time. With no
        // stop flag (the standalone diaworker binary) this is always false, so the
        // mining behavior is unchanged.
        if should_stop(stop_flag) {
            return;
        }
        let ctn = Instant::now();
        // println!("- nonce_start: {}", nonce_start);
        let mut result = do_diamond_group_mining(
            current_mining_number,
            &current_mining_block_hash,
            &rwd_addr,
            &custom_nonce,
            nonce_start,
            nonce_space,
        );
        // println!("do_diamond_group_mining: {:?}", &result);
        let use_secs = Instant::now().duration_since(ctn).as_millis() as f64 / 1000.0;
        result.use_secs = use_secs;
        result.is_gpu = false;
        result.gpu_batch_ok = true;
        if !send_diamond_result(&result_ch_tx, result) {
            return;
        }
        let Some(ns) = nonce_start.checked_add(nonce_space) else {
            break; // u64 nonce end
        };
        nonce_start = ns;
        if use_secs.is_finite() && use_secs > 0.0 {
            nonce_space = (nonce_space as f64 / use_secs * MINING_INTERVAL) as u64;
        }
        nonce_space = nonce_space.max(1);

        // check next
        if current_mining_number < MINING_DIAMOND_NUM.load(Acquire) {
            return; // turn to next number
        }
    }
}

#[cfg(feature = "ocl")]
fn run_diamond_worker_thread_opencl(
    cnf: &DiaWorkConf,
    _thrid: usize,
    result_ch_tx: mpsc::SyncSender<DiamondMiningResult>,
    gpu: std::sync::Arc<OpenclGpuHandle>,
    stop_flag: &Option<Arc<AtomicBool>>,
) {
    if mining_is_gated(&cnf.runtime, &cnf.efficiency) {
        delay_return_ms!(2000);
    }
    let cmdn = MINING_DIAMOND_NUM.load(Acquire);
    if cmdn == 0 {
        delay_return_ms!(99); // not yet
    }

    let rwd_addr = cnf.rewardaddr.clone();
    let current_mining_number: u32 = cmdn;
    let current_mining_block_hash: Hash = {
        MINING_DIAMOND_STUFF
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    };

    let mut custom_nonce = [0u8; HASH_WIDTH];
    if let Err(e) = getrandom::fill(&mut custom_nonce) {
        eprintln!("[Mining] Secure random nonce failed: {e}");
        return;
    }
    let custom_nonce = Hash::from(custom_nonce);
    let mut nonce_start = 0;

    loop {
        // Same reason as the CPU worker: acknowledge shutdown without waiting for
        // the diamond number to turn over.
        if should_stop(stop_flag) {
            return;
        }
        let wg_cap = gpu.workgroups(cnf.workgroups, cnf.runtime.thermal_workgroups_cap());
        let gpu_nonce_space = (wg_cap as u64)
            .saturating_mul(cnf.localsize as u64)
            .saturating_mul(cnf.unitsize as u64);
        let ctn = Instant::now();
        let mut result = {
            let opencl = gpu.lock_resources();
            do_diamond_group_mining_opencl(
                &opencl,
                current_mining_number,
                &current_mining_block_hash,
                &rwd_addr,
                &custom_nonce,
                nonce_start,
                gpu_nonce_space,
                wg_cap,
                cnf.localsize,
                cnf.unitsize,
            )
        };
        result.is_gpu = true;
        result.nonce_space = gpu_nonce_space;
        let use_secs = Instant::now().duration_since(ctn).as_millis() as f64 / 1000.0;
        result.use_secs = use_secs;
        if !result.gpu_batch_ok {
            gpu.on_batch_error(
                GpuBatchError::Other("diamond OpenCL batch failed".into()),
                cnf.efficiency.oom_fallback,
                cnf.workgroups,
                &cnf.runtime,
            );
            delay_return_ms!(50);
        }
        gpu.on_batch_success(cnf.workgroups, &cnf.runtime);
        if !send_diamond_result(&result_ch_tx, result) {
            return;
        }

        let Some(ns) = nonce_start.checked_add(gpu_nonce_space) else {
            break;
        };
        nonce_start = ns;

        if current_mining_number < MINING_DIAMOND_NUM.load(Acquire) {
            return;
        }
    }
}

fn do_diamond_group_mining(
    number: u32,
    prevblockhash: &Hash,
    rwdaddr: &Address,
    custom_message: &Hash,
    nonce_start: u64,
    nonce_space: u64,
) -> DiamondMiningResult {
    let empthbytes = [0u8; 0];
    let prevhash: &[u8; HASH_WIDTH] = prevblockhash;
    let address: &[u8; 21] = rwdaddr;
    let custom_nonce: &[u8] = maybe!(
        number > DIAMOND_ABOVE_NUMBER_OF_CREATE_BY_CUSTOM_MESSAGE,
        custom_message.as_bytes(),
        &empthbytes
    );
    let mut most = DiamondMiningResult {
        number,
        nonce_start,
        nonce_space,
        u64_nonce: 0,
        msg_nonce: custom_nonce.to_vec(),
        dia_str: [b'W'; DIAMOND_HASH_LEN],
        is_success: None,
        use_secs: 0.0,
        is_gpu: false,
        gpu_batch_ok: true,
    };
    let mut most_firhx = [0u8; HASH_WIDTH];
    let mut most_resxh = [0u8; HASH_WIDTH];
    let mut most_diastr = [b'W'; DIAMOND_HASH_LEN];
    let mut most_noncebytes = [0u8; 8];

    // start mining
    for nonce in nonce_start..nonce_start.saturating_add(nonce_space) {
        // std::thread::sleep(std::time::Duration::from_micros(333)); // test
        let nonce_bytes = nonce.to_be_bytes();
        let (firhx, resxh, diastr) =
            x16rs::mine_diamond(number, prevhash, &nonce_bytes, address, custom_nonce);
        // A valid diamond has EXACTLY DMD_L leading zeros followed by a non-zero name. The
        // "most powerful" heuristic below maximises leading zeros, which overshoots into invalid
        // territory once difficulty is low (LOCAL TESTNET only). Test each candidate for validity
        // directly and take the first one that actually qualifies.
        if x16rs::check_diamond_hash_result(&diastr).is_some()
            && x16rs::check_diamond_difficulty(number, &firhx, &resxh)
        {
            most.u64_nonce = nonce;
            most.dia_str = diastr.clone();
            most_firhx = firhx;
            most_resxh = resxh;
            most_diastr = diastr;
            most_noncebytes = nonce_bytes;
            break;
        }
        if diamond_more_power(&diastr, &most.dia_str) {
            most.u64_nonce = nonce;
            most.dia_str = diastr.clone();
            most_firhx = firhx;
            most_resxh = resxh;
            most_diastr = diastr;
            most_noncebytes = nonce_bytes;
        }
        // next
    }
    // check success
    if let Some(dia_name) = check_diamer_success(number, most_firhx, most_resxh, most_diastr) {
        let name = DiamondName::from(dia_name);
        let number = DiamondNumber::from(number);
        let mut diamint = DiamondMint::with(name, number);
        diamint.d.prev_hash = prevblockhash.clone();
        diamint.d.nonce = Fixed8::from(most_noncebytes);
        diamint.d.address = rwdaddr.clone();
        diamint.d.custom_message = custom_message.clone();
        most.is_success = Some(diamint); // mark success
    }
    // ok
    most
}

pub(crate) fn check_diamer_success(
    number: u32,
    firhx: [u8; HASH_WIDTH],
    resxh: [u8; HASH_WIDTH],
    diastr: [u8; DIAMOND_HASH_LEN],
) -> Option<[u8; 6]> {
    // The 6-char name is derived by x16rs from positions DMD_L..DMD_M of the diamond
    // string; take its result directly instead of hand-slicing so this stays correct
    // no matter the leading-zero prefix length (mainnet DMD_L=10, DMD_M=16).
    let Some(name) = x16rs::check_diamond_hash_result(&diastr) else {
        return None;
    };
    if !x16rs::check_diamond_difficulty(number, &firhx, &resxh) {
        return None;
    }
    // success find a diamond

    flush!("\n\n▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒\n");
    flush!(
        "▒▒▒▒ MINING SUCCESS: {} ({})",
        String::from_utf8_lossy(&diastr),
        number
    );
    flush!("\n▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔\n");
    Some(name)
}

fn load_init(cnf: &mut DiaWorkConf) {
    let urlapi_pending = format!("http://{}/query/diamondminer/init", &cnf.rpcaddr);
    loop {
        let body =
            match crate::rpc_http::get_text(&HTTP_CLIENT, &urlapi_pending, &cnf.api_token, None) {
                Ok(t) => t,
                Err(e) => {
                    println!(
                        "Error: cannot init diamond miner from {}: {}",
                        &urlapi_pending, e
                    );
                    delay_continue!(30);
                }
            };
        let Ok(res) = serde_json::from_str::<JV>(&body) else {
            println!("Error: invalid JSON from {urlapi_pending}");
            delay_continue!(30);
        };
        let jstr = |k| res[k].as_str().unwrap_or("");
        let err = jstr("err");
        if err.len() > 0 {
            println!("{} Error: {}", &urlapi_pending, err);
            delay_continue!(30);
        }
        let adr1 = jstr("bid_address");
        let Ok(bid_addr) = Address::from_readable(&adr1) else {
            println!("Error: bid_address '{}' format invalid", &adr1);
            delay_continue!(30);
        };
        let adr2 = jstr("reward_address");
        let Ok(rwd_addr) = Address::from_readable(&adr2) else {
            println!("Error: reward_address '{}' format invalid", &adr2);
            delay_continue!(30);
        };
        println!(
            "[Config] query diamond miner bid address: {}, reward address: {}",
            &adr1, &adr2
        );
        // ok
        cnf.bidaddr = bid_addr;
        cnf.rewardaddr = rwd_addr;
        break;
    }
    // ok
}

fn pull_and_push_diamond(cnf: &DiaWorkConf) {
    let mining_num = MINING_DIAMOND_NUM.load(Acquire);

    let urlapi_latest = format!("http://{}/query/latest", &cnf.rpcaddr);
    // get next number
    // println!("urlapi_latest: {}", &urlapi_latest);
    let body = match crate::rpc_http::get_text(&HTTP_CLIENT, &urlapi_latest, &cnf.api_token, None) {
        Ok(t) => t,
        Err(e) => {
            println!("Error: cannot get latest from {}: {}", &urlapi_latest, e);
            delay_return!(30);
        }
    };
    let Ok(res) = serde_json::from_str::<JV>(&body) else {
        println!("Error: invalid JSON from {urlapi_latest}");
        delay_return!(30);
    };
    // println!("get latest: {:?}", &res);
    let jnum = |k| res[k].as_u64().unwrap_or(0);
    let next_num = jnum("diamond") as u32 + 1;
    // println!("mining next num: {} {}", &mining_num, &next_num);
    if next_num == 1 {
        // println!("get latest: next_num == 1");
        *MINING_DIAMOND_STUFF
            .write()
            .unwrap_or_else(|e| e.into_inner()) = genesis_block_hash();
        // Release: publish the STUFF write above before the number, so a reader that
    // sees this number (with an Acquire load) also sees the matching prev_hash.
    MINING_DIAMOND_NUM.store(next_num, Release);
        return; // first mining
    }
    if next_num <= mining_num {
        return; // no change
    }
    // query next!
    let urlapi_diamond = format!(
        "http://{}/query/diamond?number={}",
        &cnf.rpcaddr,
        next_num - 1
    );
    // println!("urlapi_diamond: {}", &urlapi_diamond);
    let body = match crate::rpc_http::get_text(&HTTP_CLIENT, &urlapi_diamond, &cnf.api_token, None)
    {
        Ok(t) => t,
        Err(e) => {
            println!("Error: cannot get diamond from {}: {}", &urlapi_diamond, e);
            delay_return!(30);
        }
    };
    let Ok(res) = serde_json::from_str::<JV>(&body) else {
        println!("Error: invalid JSON from {urlapi_diamond}");
        delay_return!(30);
    };
    // println!("query diamond: {:?}", &res);
    let prev_hash = res["born"]["hash"].as_str().unwrap_or("");
    let Ok(hx) = hex::decode(&prev_hash) else {
        println!(
            "Error: cannot get born.hash from {}: {:?}",
            &urlapi_diamond, &res
        );
        delay_return!(30); // hash error
    };
    if hx.len() != HASH_WIDTH {
        delay_return!(30); // hash error
    }
    // change stuff
    let Ok(hash_bytes) = hx.try_into() else {
        delay_return!(30);
    };
    *MINING_DIAMOND_STUFF
        .write()
        .unwrap_or_else(|e| e.into_inner()) = Hash::from(hash_bytes);
    // Release: publish the STUFF write above before the number, so a reader that
    // sees this number (with an Acquire load) also sees the matching prev_hash.
    MINING_DIAMOND_NUM.store(next_num, Release);
    // print first req msg
    if mining_num == 0 {
        may_print_turn_to_nex_diamond_mining(mining_num, None);
    }
}

fn push_diamond_mining_success(cnf: &DiaWorkConf, success: DiamondMint) {
    let urlapi_success = format!("http://{}/submit/diamondminer/success", &cnf.rpcaddr);
    let actionbody = success.serialize();
    // println!("\n\ncurl {}?hexbody=true -X POST -d '{}'", &urlapi_success, &actionbody.to_hex());
    // Submitting the mined diamond is the whole payoff, and the result was already
    // drained from the mining channel, so a single transient network error must not
    // silently lose it. Retry transport failures with backoff; stop as soon as the
    // node returns a response (accept or deterministic rejection), mirroring the
    // block submit path.
    const MAX_SUBMIT_ATTEMPTS: u32 = 5;
    let mut body = String::new();
    let mut got_response = false;
    for attempt in 1..=MAX_SUBMIT_ATTEMPTS {
        match crate::rpc_http::post_text(
            &HTTP_CLIENT,
            &urlapi_success,
            &cnf.api_token,
            actionbody.clone(),
        ) {
            Ok(t) => {
                body = t;
                got_response = true;
                break;
            }
            Err(e) => {
                println!(
                    "Error: attempt {attempt}/{MAX_SUBMIT_ATTEMPTS} cannot submit diamond success to {urlapi_success}: {e}"
                );
                if attempt < MAX_SUBMIT_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(500u64 * attempt as u64));
                }
            }
        }
    }
    if !got_response {
        println!(
            "ㄨㄨㄨㄨ Failed submit tx diamond mint to mainnet after {MAX_SUBMIT_ATTEMPTS} attempts (network unreachable). Check the node/connection."
        );
        return;
    }
    let Ok(res) = serde_json::from_str::<JV>(&body) else {
        println!("Error: invalid JSON from {urlapi_success}");
        return;
    };
    let jstr = |k: &str| res[k].as_str().unwrap_or("");
    let tx_err = jstr("err");
    if tx_err.len() > 0 {
        println!(
            "ㄨㄨㄨㄨ Failed submit tx diamond mint to mainnet\n     ERROR: {}\n",
            tx_err
        );
        return;
    }
    let tx_hash = jstr("tx_hash");
    if tx_hash.len() != 64 {
        return; // err
    }
    println!(
        "Success submit tx diamond mint {} ({}) to mainnet, \n        get tx hash: {}\n",
        success.d.diamond.to_readable(),
        *success.d.number,
        tx_hash
    );
}

#[cfg(test)]
mod result_channel_tests {
    use super::*;

    fn mined_diamond() -> DiamondMint {
        DiamondMint::with(DiamondName::from(*b"ABCDEF"), DiamondNumber::from(1u32))
    }

    fn statistics_result(number: u32) -> DiamondMiningResult {
        let mut res = DiamondMiningResult::default();
        res.number = number;
        res
    }

    fn success_result(number: u32) -> DiamondMiningResult {
        let mut res = statistics_result(number);
        res.is_success = Some(mined_diamond());
        res
    }

    #[test]
    fn a_full_diamond_queue_drops_statistics_but_never_a_mined_diamond() {
        let (tx, rx) = mpsc::sync_channel::<DiamondMiningResult>(1);
        assert!(send_diamond_result(&tx, statistics_result(5)));
        // Queue is full now: a statistics-only batch is dropped, not blocked.
        assert!(send_diamond_result(&tx, statistics_result(6)));
        assert_eq!(rx.try_recv().map(|r| r.number), Ok(5));

        assert!(send_diamond_result(&tx, success_result(9)));
        assert_eq!(rx.try_recv().map(|r| r.number), Ok(9));
        drop(rx);
        assert!(!send_diamond_result(&tx, success_result(9)));
    }

    #[test]
    fn a_mined_diamond_waits_for_queue_space_instead_of_being_dropped() {
        let (tx, rx) = mpsc::sync_channel::<DiamondMiningResult>(1);
        assert!(send_diamond_result(&tx, statistics_result(1)));

        // The queue is full and this result is a payout, so the worker must block
        // until the drain makes room rather than throw the diamond away.
        let sender = spawn(move || send_diamond_result(&tx, success_result(42)));
        assert_eq!(rx.recv().map(|r| r.number), Ok(1));
        assert!(sender.join().unwrap());
        let delivered = rx.recv().unwrap();
        assert_eq!(delivered.number, 42);
        assert!(delivered.is_success.is_some());
    }

    #[test]
    fn a_panicking_drain_iteration_never_ends_the_diamond_result_thread() {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let (tx, rx) = mpsc::sync_channel::<DiamondMiningResult>(4);
        let mut drained = 0u32;
        for round in 0..3u32 {
            assert!(send_diamond_result(&tx, statistics_result(round)));
            guard_mining_iteration("diamond result test loop", || {
                while rx.try_recv().is_ok() {
                    drained += 1;
                }
                if round == 1 {
                    panic!("simulated diamond result thread panic");
                }
            });
        }
        std::panic::set_hook(previous_hook);
        assert_eq!(drained, 3);
    }

    #[test]
    fn a_tracked_diamond_thread_acknowledges_shutdown_when_it_exits() {
        // The shutdown supervisor waits on active_mining_threads(), so a HACD
        // thread must be counted before it starts and must release the count when
        // it returns.
        let runtime = MiningRuntimeState::new(0, 1);
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        spawn_tracked_diamond_thread(&runtime, move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
        });
        assert_eq!(started_rx.recv(), Ok(()));
        assert_eq!(runtime.active_mining_threads(), 1);

        drop(release_tx);
        let deadline = Instant::now() + Duration::from_secs(5);
        while runtime.active_mining_threads() > 0 && Instant::now() < deadline {
            sleep(Duration::from_millis(5));
        }
        assert_eq!(runtime.active_mining_threads(), 0);
    }

    #[test]
    fn a_stop_flag_is_observed_by_the_diamond_loops() {
        assert!(!should_stop(&None));
        let flag = Arc::new(AtomicBool::new(false));
        let stop = Some(flag.clone());
        assert!(!should_stop(&stop));
        flag.store(true, Relaxed);
        assert!(should_stop(&stop));
    }
}

fn run_diamond_mining_benchmark(cnf: &DiaWorkConf, config_path: &str) {
    #[cfg(not(feature = "ocl"))]
    {
        let _ = (cnf, config_path);
        println!("[benchmark] Rebuild diaworker with --features ocl");
        return;
    }
    #[cfg(feature = "ocl")]
    {
        if !cnf.useopencl {
            println!("[benchmark] HACD is CPU-only; Auto Tune applies to the HAC poworker.");
            return;
        }
        println!(
            "[benchmark] HACD: GPU tuning uses same profiles as HAC; run poworker benchmark or share ini."
        );
        let scan = crate::opencl_diag::scan_opencl();
        let init_unitsize = cnf.unitsize.max(128);
        let opencl_resources = initialize_opencl(
            true,
            &cnf.opencldir,
            &cnf.platformid,
            &cnf.deviceids,
            &cnf.workgroups,
            &cnf.localsize,
            &init_unitsize,
            Some(&scan),
            false,
        );
        if opencl_resources.is_empty() {
            return;
        }
        let opencl = &opencl_resources[0];
        let limits = crate::gpu_arch::ArchLimits::for_slug(&opencl.arch_slug);
        let min_wg = limits.panel_min_wg.min(opencl.workgroups);
        let max_wg = opencl.workgroups;
        let max_us = limits
            .max_unit_size()
            .min(opencl.allocated_unitsize)
            .max(32);
        let candidates = benchmark_candidates_for_device(
            opencl.vendor,
            min_profile_tier_for_mode(cnf.efficiency.mode),
            crate::gpu_arch::ArchLimits::panel_max_tier(&cnf.gpu_slug),
            min_wg,
            max_wg,
            max_us,
        );
        if candidates.is_empty() {
            println!("[benchmark] HACD: no safe tuning candidates");
            return;
        }
        let per =
            (cnf.efficiency.benchmark_seconds.max(15) as u64 / candidates.len() as u64).max(4);
        let prev = Hash::default();
        let addr = cnf.rewardaddr.clone();
        let msg = Hash::default();
        let mut best: Option<(BenchmarkPick, f64)> = None;
        for pick in candidates {
            let batch = pick.workgroups as u64 * cnf.localsize as u64 * pick.unitsize as u64;
            let deadline = Instant::now() + Duration::from_secs(per);
            let mut total = 0u64;
            let mut secs = 0.0f64;
            let mut nonce = 0u64;
            while Instant::now() < deadline {
                let ctn = Instant::now();
                let res = do_diamond_group_mining_opencl(
                    opencl,
                    1,
                    &prev,
                    &addr,
                    &msg,
                    nonce,
                    batch,
                    pick.workgroups,
                    cnf.localsize,
                    pick.unitsize,
                );
                if res.gpu_batch_ok {
                    total += batch;
                }
                secs += ctn.elapsed().as_secs_f64();
                nonce = nonce.wrapping_add(batch);
            }
            let hps = if secs > 0.0 { total as f64 / secs } else { 0.0 };
            let watts = cnf.efficiency.estimate_tuning_watts(
                &pick.profile,
                pick.workgroups,
                pick.unitsize,
                max_wg,
                max_us,
            );
            let kh_per_j = if watts > 0.0 {
                hps / watts / 1000.0
            } else {
                0.0
            };
            let score = match cnf.efficiency.mode {
                EfficiencyMode::Max => hps,
                _ => kh_per_j,
            };
            println!(
                "[benchmark] HACD {}: {} ({:.1} kH/J, wg={}, unit_size={})",
                pick.profile,
                rates_to_show(hps),
                kh_per_j,
                pick.workgroups,
                pick.unitsize
            );
            if hps > 0.0
                && best
                    .as_ref()
                    .map(|(_, value)| score > *value)
                    .unwrap_or(true)
            {
                best = Some((pick, score));
            }
        }
        if let Some((pick, _)) = best {
            let _ = apply_benchmark_pick(config_path, &pick);
        } else {
            println!("[benchmark] HACD: all tuning points failed; config unchanged");
        }
    }
}
