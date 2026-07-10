use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering::*};
use std::sync::{RwLock, mpsc};

use std::thread::*;
use std::time::*;

use reqwest::blocking::Client as HttpClient;
use serde_json::Value as JV;

use crate::efficiency::*;

use basis::difficulty::*;
use field::*;
use mint::action::*;
use mint::genesis::*;
use sys::*;

use crate::hash_util::diamond_more_power;

#[cfg(feature = "ocl")]
use crate::gpu_oom::GpuBatchError;
#[cfg(feature = "ocl")]
use crate::opencl_gpu::{initialize_opencl, opencl_snapshot_from_resource, OpenclGpuHandle};
#[cfg(feature = "ocl")]
#[path = "opencl_dia.rs"]
mod opencl_dia;
#[cfg(feature = "ocl")]
use opencl_dia::do_diamond_group_mining_opencl;

/*************************************/

#[derive(Clone)]
pub struct DiaWorkConf {
    pub rpcaddr: String,
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
    pub efficiency: EfficiencyConf,
    pub runtime: Arc<MiningRuntimeState>,
}

impl DiaWorkConf {
    pub fn new(ini: &IniObj) -> DiaWorkConf {
        let sec = &ini_section(ini, "default"); // default = root
        let sec_gpu = &ini_section(ini, "gpu");
        let efficiency = EfficiencyConf::from_ini(ini);
        let tuning = resolve_gpu_tuning(sec_gpu, &efficiency);
        let configured_supervene = ini_must_u64(sec, "supervene", 2) as u32;
        let active = efficiency.initial_active_supervene(configured_supervene);
        let runtime = MiningRuntimeState::new(tuning.workgroups, active);
        let cnf = DiaWorkConf {
            rpcaddr: ini_must(sec, "connect", "127.0.0.1:8081"),
            supervene: configured_supervene,
            bidaddr: Address::default(),
            rewardaddr: Address::default(),
            useopencl: ini_must_bool(sec_gpu, "use_opencl", false) as bool,
            workgroups: tuning.workgroups,
            localsize: ini_must_u64(sec_gpu, "local_size", 256) as u32,
            unitsize: tuning.unitsize,
            opencldir: ini_must(sec_gpu, "opencl_dir", "opencl/"),
            debug: ini_must_u64(sec_gpu, "debug", 0) as u32,
            platformid: ini_must_u64(sec_gpu, "platform_id", 0) as u32,
            deviceids: ini_must(sec_gpu, "device_ids", ""),
            cpu_assist: ini_must_bool(sec_gpu, "cpu_assist", true) as bool,
            gpu_profile: tuning.profile,
            efficiency,
            runtime,
        };
        cnf
    }
}

/*************************************/

const HASH_WIDTH: usize = 32;
const MINING_INTERVAL: f64 = 3.0; // 3 secs

// current mining diamond number
static MINING_DIAMOND_NUM: AtomicU32 = AtomicU32::new(0);

use std::sync::LazyLock;
static HTTP_CLIENT: LazyLock<HttpClient> = LazyLock::new(|| HttpClient::new());
static MINING_DIAMOND_STUFF: LazyLock<RwLock<Hash>> = LazyLock::new(|| RwLock::default());

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub(crate) struct DiamondMiningResult {
    number: u32,
    nonce_start: u64,
    nonce_space: u64,
    u64_nonce: u64,
    msg_nonce: Vec<u8>,
    dia_str: [u8; 16],
    is_success: Option<DiamondMint>,
    use_secs: f64,
    is_gpu: bool,
    gpu_batch_ok: bool,
}

/*
* Diamond worker
*/
pub fn diaworker() {
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

    let (res_tx, res_rx) = mpsc::channel();

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

    // deal results
    let cnf1 = cnf.clone();
    spawn(move || {
        let mut most_dia_str = [b'W'; 16];
        let mut rstx = res_rx;
        loop {
            deal_diamond_mining_results(&cnf1, &mut most_dia_str, &mut rstx, vene);
            delay_continue_ms!(77);
        }
    });

    // start worker
    if cnf.useopencl {
        // opencl is enabled
        #[cfg(feature = "ocl")]
        {
            // Initialize OpenCL
            println!("\n[Start] Create GPU diamond miner worker #{}.", opencl_resources.len());
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
                let rstx: mpsc::Sender<DiamondMiningResult> = res_tx.clone();
                spawn(move || {
                    loop {
                        run_diamond_worker_thread_opencl(
                            &cnf2,
                            thrid,
                            rstx.clone(),
                            gpu.clone(),
                        );
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
                spawn(move || {
                    loop {
                        run_diamond_worker_thread(&cnf2, thrid, rstx.clone());
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
                    spawn(move || {
                        loop {
                            run_diamond_worker_thread(&cnf2, thrid, rstx.clone());
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
            spawn(move || {
                loop {
                    run_diamond_worker_thread(&cnf2, thrid, rstx.clone());
                    delay_continue_ms!(9);
                }
            });
        }
    }

    // pull loop
    loop {
        if !is_within_idle_schedule(
            cnf.efficiency.idle_start_hour,
            cnf.efficiency.idle_end_hour,
        ) {
            delay_continue!(5);
            continue;
        }
        if cnf.runtime.paused_unprofitable.load(Relaxed) {
            delay_continue!(3);
            continue;
        }
        cnf.runtime.apply_thermal_throttle(
            cnf.efficiency.max_temp_c,
            cnf.efficiency.throttle_workgroups,
            &cnf.efficiency.thermal_file,
            cnf.efficiency.thermal_gpu_index,
        );
        pull_and_push_diamond(&cnf);
        delay_continue!(MINING_INTERVAL as u64);
    }
}

fn deal_diamond_mining_results(
    cnf: &DiaWorkConf,
    most_dia_str: &mut [u8; 16],
    result_ch_rx: &mut mpsc::Receiver<DiamondMiningResult>,
    vene: u32,
) {
    let mut deal_number = 0u32;
    let mut most = DiamondMiningResult::default();
    most.dia_str = [b'w'; 16];
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
            push_diamond_mining_success(cnf, success.clone());
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
    let diastr = String::from_utf8(most.dia_str.to_vec()).unwrap();
    let most_diastr = String::from_utf8(most_dia_str.to_vec()).unwrap();
    let avg_use_secs = total_use_secs / recv_count as f64;
    let nonce_rates = if avg_use_secs.is_finite() && avg_use_secs > 0.0 {
        total_nonce_space as f64 / avg_use_secs
    } else {
        0.0
    };
    let active_cpu = cnf.runtime.active_cpu_assist.load(Relaxed);
    cnf.runtime.maybe_adjust_supervene(&cnf.efficiency, gpu_nonce_space, cpu_nonce_space);
    if should_pause_for_diamond_profit(&cnf.efficiency, &cnf.gpu_profile, active_cpu) {
        cnf.runtime.paused_unprofitable.store(true, Relaxed);
        println!(
            "\n[efficiency] HACD mining paused — daily power cost exceeds configured revenue target (hac_price)."
        );
    } else {
        cnf.runtime.paused_unprofitable.store(false, Relaxed);
    }
    let paused = cnf.runtime.paused_unprofitable.load(Relaxed);
    let gpu_w = cnf.efficiency.estimate_gpu_watts(&cnf.gpu_profile);
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
            gpu_hashrate: nonce_rates,
            cpu_hashrate: 0.0,
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

fn may_print_turn_to_nex_diamond_mining(curr_number: u32, most_dia_str: Option<&mut [u8; 16]>) {
    let mining_number = MINING_DIAMOND_NUM.load(Relaxed);
    if mining_number <= curr_number {
        return; // not turn
    }
    if let Some(most_dia_str) = most_dia_str {
        *most_dia_str = [b'W'; 16]; // reset 
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
    thrid: usize,
    result_ch_tx: mpsc::Sender<DiamondMiningResult>,
) {
    if mining_is_gated(&cnf.runtime, &cnf.efficiency) {
        delay_return_ms!(2000);
        return;
    }
    let cmdn = MINING_DIAMOND_NUM.load(Relaxed);
    if cmdn == 0 {
        delay_return_ms!(99); // not yet
    }
    #[cfg(feature = "ocl")]
    if cnf.useopencl && cnf.cpu_assist {
        let active = cnf.runtime.active_cpu_assist.load(Relaxed);
        if (thrid as u32) >= active {
            delay_return_ms!(400);
            return;
        }
    }

    let rwd_addr = cnf.rewardaddr.clone();

    let mut nonce_space: u64 = 15000;
    let current_mining_number: u32 = cmdn;
    let current_mining_block_hash: Hash = { MINING_DIAMOND_STUFF.read().unwrap().clone() };

    // start mining
    let mut custom_nonce = [0u8; HASH_WIDTH];
    getrandom::fill(&mut custom_nonce).unwrap();
    let custom_nonce = Hash::from(custom_nonce);
    // Note: All threads starting from nonce_start = 0 here is not a bug:
    // each thread/task has been assigned a random custom_nonce above,
    // so x16rs::mine_diamond input differs; even with the same nonce_start,
    // the actual search hash space is disjoint and no hashrate conflict occurs.
    let mut nonce_start = 0;

    loop {
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
        result_ch_tx.send(result).unwrap(); // channel send
        let ns = nonce_start.checked_add(nonce_space);
        if let None = ns {
            break; // u64 nonce end
        }
        nonce_start = ns.unwrap();
        if use_secs.is_finite() && use_secs > 0.0 {
            nonce_space = (nonce_space as f64 / use_secs * MINING_INTERVAL) as u64;
        }
        nonce_space = nonce_space.max(1);

        // check next
        if current_mining_number < MINING_DIAMOND_NUM.load(Relaxed) {
            return; // turn to next number
        }
    }
}

#[cfg(feature = "ocl")]
fn run_diamond_worker_thread_opencl(
    cnf: &DiaWorkConf,
    _thrid: usize,
    result_ch_tx: mpsc::Sender<DiamondMiningResult>,
    gpu: std::sync::Arc<OpenclGpuHandle>,
) {
    if mining_is_gated(&cnf.runtime, &cnf.efficiency) {
        delay_return_ms!(2000);
        return;
    }
    let cmdn = MINING_DIAMOND_NUM.load(Relaxed);
    if cmdn == 0 {
        delay_return_ms!(99); // not yet
    }

    let rwd_addr = cnf.rewardaddr.clone();
    let current_mining_number: u32 = cmdn;
    let current_mining_block_hash: Hash = { MINING_DIAMOND_STUFF.read().unwrap().clone() };

    let mut custom_nonce = [0u8; HASH_WIDTH];
    getrandom::fill(&mut custom_nonce).unwrap();
    let custom_nonce = Hash::from(custom_nonce);
    let mut nonce_start = 0;

    loop {
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
            return;
        }
        gpu.on_batch_success(cnf.workgroups, &cnf.runtime);
        result_ch_tx.send(result).unwrap();

        let ns = nonce_start.checked_add(gpu_nonce_space);
        if let None = ns {
            break;
        }
        nonce_start = ns.unwrap();

        if current_mining_number < MINING_DIAMOND_NUM.load(Relaxed) {
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
        dia_str: [b'W'; 16],
        is_success: None,
        use_secs: 0.0,
        is_gpu: false,
        gpu_batch_ok: true,
    };
    let mut most_firhx = [0u8; HASH_WIDTH];
    let mut most_resxh = [0u8; HASH_WIDTH];
    let mut most_diastr = [b'W'; 16];
    let mut most_noncebytes = [0u8; 8];

    // start mining
    for nonce in nonce_start..nonce_start + nonce_space {
        // std::thread::sleep(std::time::Duration::from_micros(333)); // test
        let nonce_bytes = nonce.to_be_bytes();
        let (firhx, resxh, diastr) =
            x16rs::mine_diamond(number, prevhash, &nonce_bytes, address, custom_nonce);
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
    diastr: [u8; 16],
) -> Option<[u8; 6]> {
    if let None = x16rs::check_diamond_hash_result(&diastr) {
        return None;
    }
    if !x16rs::check_diamond_difficulty(number, &firhx, &resxh) {
        return None;
    }
    // success find a diamond

    flush!("\n\n▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒\n");
    flush!(
        "▒▒▒▒ MINING SUCCESS: {} ({})",
        String::from_utf8(diastr.to_vec()).unwrap(),
        number
    );
    flush!("\n▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔\n");
    Some(diastr[10..].try_into().unwrap())
}

fn load_init(cnf: &mut DiaWorkConf) {
    let urlapi_pending = format!("http://{}/query/diamondminer/init", &cnf.rpcaddr);
    loop {
        let res = HTTP_CLIENT.get(&urlapi_pending).send();
        let Ok(repv) = res else {
            println!("Error: cannot init diamond miner from {}", &urlapi_pending);
            delay_continue!(30);
        };
        let res: JV = serde_json::from_str(&repv.text().unwrap()).unwrap();
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
    let mining_num = MINING_DIAMOND_NUM.load(Relaxed);

    let urlapi_latest = format!("http://{}/query/latest", &cnf.rpcaddr);
    // get next number
    // println!("urlapi_latest: {}", &urlapi_latest);
    let res = HTTP_CLIENT.get(&urlapi_latest).send();
    let Ok(repv) = res else {
        println!("Error: cannot get latest from {}", &urlapi_latest);
        delay_return!(30);
    };
    let res: JV = serde_json::from_str(&repv.text().unwrap()).unwrap();
    // println!("get latest: {:?}", &res);
    let jnum = |k| res[k].as_u64().unwrap_or(0);
    let next_num = jnum("diamond") as u32 + 1;
    // println!("mining next num: {} {}", &mining_num, &next_num);
    if next_num == 1 {
        // println!("get latest: next_num == 1");
        *MINING_DIAMOND_STUFF.write().unwrap() = genesis_block_hash();
        MINING_DIAMOND_NUM.store(next_num, Relaxed);
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
    let res = HTTP_CLIENT.get(&urlapi_diamond).send();
    let Ok(repv) = res else {
        println!("Error: cannot get diamond from {}", &urlapi_diamond);
        delay_return!(30);
    };
    let res: JV = serde_json::from_str(&repv.text().unwrap()).unwrap();
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
    *MINING_DIAMOND_STUFF.write().unwrap() = Hash::from(hx.try_into().unwrap());
    MINING_DIAMOND_NUM.store(next_num, Relaxed);
    // print first req msg
    if mining_num == 0 {
        may_print_turn_to_nex_diamond_mining(mining_num, None);
    }
}

fn push_diamond_mining_success(cnf: &DiaWorkConf, success: DiamondMint) {
    let urlapi_success = format!("http://{}/submit/diamondminer/success", &cnf.rpcaddr);
    let actionbody = success.serialize();
    // println!("\n\ncurl {}?hexbody=true -X POST -d '{}'", &urlapi_success, &actionbody.to_hex());
    let res = HTTP_CLIENT.post(&urlapi_success).body(actionbody).send();
    let Ok(repv) = res else {
        return; // err
    };
    let res: JV = serde_json::from_str(&repv.text().unwrap()).unwrap();
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
            println!("[benchmark] Set use_opencl=true in [gpu]");
            return;
        }
        println!("[benchmark] HACD: GPU tuning uses same profiles as HAC — run poworker benchmark or share ini.");
        let scan = crate::opencl_diag::scan_opencl();
        let opencl_resources = initialize_opencl(
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
        if opencl_resources.is_empty() {
            return;
        }
        let opencl = &opencl_resources[0];
        let profiles = benchmark_profiles_for_vendor(opencl.vendor);
        let per = (cnf.efficiency.benchmark_seconds.max(15) as u64 / profiles.len() as u64).max(4);
        let mut best_hps = 0.0f64;
        let mut best_profit = 0.0f64;
        let mut best_hps_profile = profiles[0];
        let mut best_profit_profile = profiles[0];
        let prev = Hash::default();
        let addr = cnf.rewardaddr.clone();
        let msg = Hash::default();
        for profile in profiles {
            let (wg, us) = profile_tuning(profile);
            let wg_eff = opencl.workgroups.min(wg);
            let batch = wg_eff as u64 * cnf.localsize as u64 * us as u64;
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
                    wg_eff,
                    cnf.localsize,
                    us,
                );
                if res.gpu_batch_ok {
                    total += batch;
                }
                secs += ctn.elapsed().as_secs_f64();
                nonce = nonce.wrapping_add(batch);
            }
            let hps = if secs > 0.0 { total as f64 / secs } else { 0.0 };
            let watts = cnf.efficiency.estimate_gpu_watts(profile);
            let kh_per_j = if watts > 0.0 {
                hps / watts / 1000.0
            } else {
                0.0
            };
            println!(
                "[benchmark] HACD {}: {} ({:.1} kH/J)",
                profile,
                rates_to_show(hps),
                kh_per_j
            );
            if hps > best_hps {
                best_hps = hps;
                best_hps_profile = profile;
            }
            if kh_per_j > best_profit {
                best_profit = kh_per_j;
                best_profit_profile = profile;
            }
        }
        let best_profile = match cnf.efficiency.mode {
            EfficiencyMode::Max => best_hps_profile,
            _ => best_profit_profile,
        };
        let pick = BenchmarkPick::from_profile(best_profile);
        let _ = apply_benchmark_pick(config_path, &pick);
    }
}
