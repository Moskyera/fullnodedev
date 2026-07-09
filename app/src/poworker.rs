use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::{Arc, RwLock, mpsc};

use std::thread::*;
use std::time::*;

use reqwest::blocking::Client as HttpClient;
use serde_json::Value as JV;

use crate::efficiency::*;

use basis::difficulty::*;
use basis::interface::*;
use field::*;
use mint::TransactionCoinbase;
use mint::genesis::*;
use protocol::block::*;
use sys::*;

include! {"util.rs"}

#[cfg(feature = "ocl")]
include! {"opencl_common.rs"}
#[cfg(feature = "ocl")]
include! {"opencl_pow.rs"}

#[derive(Clone)]
enum MinerBackend {
    Cpu { assist_idx: Option<u32> },
    #[cfg(feature = "ocl")]
    Opencl(Arc<OpenCLResources>),
}

/*****************************************/

#[derive(Clone)]
pub struct PoWorkConf {
    pub rpcaddr: String,
    pub supervene: u32, // cpu core (configured)
    pub noncemax: u32,
    pub noticewait: u64,   // new block notice wait
    pub useopencl: bool,   // use opencl miner
    pub workgroups: u32,   // opencl work groups
    pub localsize: u32,    // opencl work units per work group
    pub unitsize: u32,     // opencl hashes per work unit
    pub opencldir: String, // opencl source dir
    pub debug: u32,        // enable debug mode
    pub platformid: u32,   // opencl platform id
    pub deviceids: String, // opencl device id list
    /// When OpenCL is on, also run Ryzen CPU miner threads (hybrid).
    pub cpu_assist: bool,
    pub gpu_profile: String,
    pub efficiency: EfficiencyConf,
    pub runtime: Arc<MiningRuntimeState>,
}

impl PoWorkConf {
    pub fn new(ini: &IniObj) -> PoWorkConf {
        let sec = &ini_section(ini, "default"); // default = root
        let sec_gpu = &ini_section(ini, "gpu");
        let efficiency = EfficiencyConf::from_ini(ini);
        let tuning = resolve_gpu_tuning(sec_gpu, &efficiency);
        let configured_supervene = ini_must_u64(sec, "supervene", 2) as u32;
        let active = efficiency.initial_active_supervene(configured_supervene);
        let runtime = MiningRuntimeState::new(tuning.workgroups, active);
        let cnf = PoWorkConf {
            rpcaddr: ini_must(sec, "connect", "127.0.0.1:8081"),
            supervene: configured_supervene,
            noncemax: ini_must_u64(sec, "nonce_max", u32::MAX as u64) as u32,
            noticewait: ini_must_u64(sec, "notice_wait", 45),
            useopencl: ini_must_bool(sec_gpu, "use_opencl", false) as bool,
            workgroups: tuning.workgroups,
            localsize: ini_must_u64(sec_gpu, "local_size", 256) as u32,
            unitsize: tuning.unitsize,
            opencldir: ini_must(sec_gpu, "opencl_dir", "opencl/"),
            debug: ini_must_u64(sec_gpu, "debug", 0) as u32,
            platformid: ini_must_u64(sec_gpu, "platform_id", 0) as u32,
            deviceids: ini_must(sec_gpu, "device_ids", ""),
            cpu_assist: ini_must_bool(sec_gpu, "cpu_assist", true) as bool,
            gpu_profile: tuning.profile.clone(),
            efficiency,
            runtime,
        };
        println!(
            "[efficiency] mode={} profile={} work_groups={} unit_size={} dynamic_supervene={}",
            cnf.efficiency.mode.label(),
            cnf.gpu_profile,
            cnf.workgroups,
            cnf.unitsize,
            cnf.efficiency.dynamic_supervene
        );
        cnf
    }

    /// Minimal config for integration tests.
    pub fn test_defaults(rpcaddr: String, supervene: u32, noncemax: u32) -> PoWorkConf {
        let mut cnf = PoWorkConf::new(&IniObj::new());
        cnf.rpcaddr = rpcaddr;
        cnf.supervene = supervene;
        cnf.noncemax = noncemax;
        cnf.useopencl = false;
        cnf.cpu_assist = false;
        cnf
    }
}

/*****************************************/

const HASH_WIDTH: usize = 32;
const MINING_INTERVAL: f64 = 3.0; // 3 secs
const TARGET_BLOCK_TIME: f64 = 300.0; // 5 mins
const ONEDAY_BLOCK_NUM: f64 = 288.0; // one day block

// current mining diamond number
static MINING_BLOCK_HEIGHT: AtomicU64 = AtomicU64::new(0);

use std::sync::LazyLock;
static HTTP_CLIENT: LazyLock<HttpClient> =
    LazyLock::new(|| HttpClient::builder().no_proxy().build().unwrap());
static MINING_BLOCK_STUFF: LazyLock<RwLock<Arc<BlockMiningStuff>>> =
    LazyLock::new(|| RwLock::default());

#[derive(Clone, Default)]
struct BlockMiningStuff {
    height: u64,
    target_hash: Hash,
    block_intro: BlockIntro,
    coinbase_tx: TransactionCoinbase,
    mkrl_list: Vec<Hash>,
}

#[derive(Clone, Default)]
struct BlockMiningResult {
    height: u64,
    nonce_start: u32,
    nonce_space: u32,
    gpu_nonce_space: u32,
    cpu_nonce_space: u32,
    head_nonce: u32,
    coinbase_nonce: Vec<u8>,
    result_hash: Vec<u8>,
    target_hash: Vec<u8>,
    use_secs: f64,
    is_gpu: bool,
}

impl BlockMiningResult {
    fn new() -> BlockMiningResult {
        let mut res = BlockMiningResult::default();
        res.result_hash = vec![255u8; 32];
        res
    }
}

pub fn poworker() {
    let cnfp = "./poworker.config.ini".to_string();
    let inicnf = sys::load_config(cnfp.clone());
    let cnf = PoWorkConf::new(&inicnf);
    if cnf.efficiency.benchmark_seconds > 0 {
        run_block_mining_benchmark(&cnf, &cnfp);
        return;
    }
    poworker_with_conf(cnf);
}

pub fn poworker_with_conf(cnf: PoWorkConf) {
    poworker_with_stop(cnf, None);
}

pub fn poworker_with_stop(cnf: PoWorkConf, stop_flag: Option<Arc<AtomicBool>>) {
    let (res_tx, res_rx) = mpsc::channel();

    let miner_backends = build_miner_backends(&cnf);

    // deal results
    let cnf1 = cnf.clone();
    let worker_qty = miner_backends.len();
    let stop_flag_res = stop_flag.clone();
    spawn(move || {
        let mut most_hash = vec![255u8; 32];
        let mut rstx = res_rx;
        loop {
            if should_stop(&stop_flag_res) {
                return;
            }
            deal_block_mining_results(&cnf1, &mut most_hash, &mut rstx, worker_qty);
            delay_continue_ms!(123);
        }
    });

    for (thrid, backend) in miner_backends.into_iter().enumerate() {
        let cnf2 = cnf.clone();
        let rstx = res_tx.clone();
        let stop_flag_miner = stop_flag.clone();
        spawn(move || {
            loop {
                if should_stop(&stop_flag_miner) {
                    return;
                }
                run_block_mining_item(&cnf2, thrid, rstx.clone(), backend.clone());
                delay_continue_ms!(9);
            }
        });
    }

    // loop
    loop {
        if should_stop(&stop_flag) {
            return;
        }
        if !is_within_idle_schedule(
            cnf.efficiency.idle_start_hour,
            cnf.efficiency.idle_end_hour,
        ) {
            delay_continue_ms!(5000);
            continue;
        }
        if cnf.runtime.paused_unprofitable.load(Relaxed) {
            delay_continue_ms!(3000);
            continue;
        }
        cnf.runtime.apply_thermal_throttle(
            cnf.efficiency.max_temp_c,
            cnf.efficiency.throttle_workgroups,
            &cnf.efficiency.thermal_file,
            cnf.efficiency.thermal_gpu_index,
        );
        pull_pending_block_stuff(&cnf);
        delay_continue_ms!(25);
    }
}

fn should_stop(stop_flag: &Option<Arc<AtomicBool>>) -> bool {
    stop_flag.as_ref().map(|f| f.load(Relaxed)).unwrap_or(false)
}

fn build_miner_backends(cnf: &PoWorkConf) -> Vec<MinerBackend> {
    let mut backends = Vec::new();

    if cnf.useopencl {
        #[cfg(feature = "ocl")]
        {
            let opencl_resources = initialize_opencl(
                false,
                &cnf.opencldir,
                &cnf.platformid,
                &cnf.deviceids,
                &cnf.workgroups,
                &cnf.localsize,
                &cnf.unitsize,
            );
            if !opencl_resources.is_empty() {
                println!(
                    "\n[Start] Create GPU block miner worker #{}.",
                    opencl_resources.len()
                );
                for resource in opencl_resources {
                    backends.push(MinerBackend::Opencl(Arc::new(resource)));
                }
            }
        }

        #[cfg(not(feature = "ocl"))]
        {
            println!(
                "\n[Warn] use_opencl=true but app built without `ocl` feature, fallback to CPU miner."
            );
        }

        if cnf.cpu_assist && cnf.supervene > 0 && !backends.is_empty() {
            let thrnum = cnf.efficiency.spawn_supervene(cnf.supervene) as usize;
            println!(
                "\n[Start] Create #{} Ryzen CPU assist threads (hybrid GPU+CPU, active={}).",
                thrnum,
                cnf.runtime.active_cpu_assist.load(Relaxed)
            );
            for i in 0..thrnum {
                backends.push(MinerBackend::Cpu {
                    assist_idx: Some(i as u32),
                });
            }
        }
    }

    if backends.is_empty() {
        let thrnum = cnf.efficiency.clamp_supervene(cnf.supervene.max(1)) as usize;
        println!(
            "\n[Start] Create #{} CPU block miner worker thread.",
            thrnum
        );
        for _ in 0..thrnum {
            backends.push(MinerBackend::Cpu { assist_idx: None });
        }
    }

    backends
}

fn run_block_mining_item(
    _cnf: &PoWorkConf,
    _thrid: usize,
    result_ch_tx: mpsc::Sender<Arc<BlockMiningResult>>,
    backend: MinerBackend,
) {
    if mining_is_gated(&_cnf.runtime, &_cnf.efficiency) {
        delay_return_ms!(2000);
        return;
    }

    let mining_hei = MINING_BLOCK_HEIGHT.load(Relaxed);
    if mining_hei == 0 {
        delay_return_ms!(111); // not yet
    }

    let mut coinbase_nonce = [0u8; HASH_WIDTH];
    getrandom::fill(&mut coinbase_nonce).unwrap();
    let coinbase_nonce = Hash::from(coinbase_nonce);
    // Note: All threads starting from nonce_start = 0 here is not a bug:
    // each thread/task has been assigned a random coinbase_nonce above,
    // so block_intro (block header hash) differs; even with the same nonce_start,
    // the actual search hash space is disjoint and no hashrate conflict occurs.
    if let MinerBackend::Cpu { assist_idx: Some(idx) } = &backend {
        let active = _cnf.runtime.active_cpu_assist.load(Relaxed);
        if *idx >= active {
            delay_return_ms!(400);
            return;
        }
    }

    let mut nonce_start: u32 = 0;
    let nonce_limit = _cnf.noncemax.max(1);
    let mut nonce_space: u32 = match &backend {
        MinerBackend::Cpu { .. } => 100000,
        #[cfg(feature = "ocl")]
        MinerBackend::Opencl(res) => {
            let wg = _cnf
                .runtime
                .workgroups(res.workgroups.min(_cnf.workgroups));
            wg * _cnf.localsize * _cnf.unitsize
        }
    };
    #[cfg(feature = "ocl")]
    let is_gpu_backend = matches!(backend, MinerBackend::Opencl(_));
    #[cfg(not(feature = "ocl"))]
    let is_gpu_backend = false;
    // stuff data
    let stuff = { MINING_BLOCK_STUFF.read().unwrap().clone() };
    let height = stuff.height;
    let mut coinbase_tx = stuff.coinbase_tx.clone();
    coinbase_tx.set_nonce(coinbase_nonce);
    let mut block_intro = stuff.block_intro.clone();
    block_intro.set_mrklroot(calculate_mrkl_prelude_update(
        coinbase_tx.hash(),
        &stuff.mkrl_list,
    ));
    loop {
        if nonce_start >= nonce_limit {
            return;
        }

        let remain = nonce_limit.saturating_sub(nonce_start);
        let current_nonce_space = nonce_space.min(remain).max(1);
        let ctn = Instant::now();
        let block_intro_bin = block_intro.serialize();

        let (head_nonce, result_hash, gpu_ns, cpu_ns) = match &backend {
            MinerBackend::Cpu { .. } => {
                let (hn, rh) =
                    do_group_block_mining(height, block_intro_bin, nonce_start, current_nonce_space);
                (hn, rh, 0u32, current_nonce_space)
            }
            #[cfg(feature = "ocl")]
            MinerBackend::Opencl(opencl) => {
                let wg_cap = _cnf
                    .runtime
                    .workgroups(opencl.workgroups.min(_cnf.workgroups));
                let unit_batch = (_cnf.localsize as u64) * (_cnf.unitsize as u64);
                if wg_cap == 0 || unit_batch == 0 {
                    let (hn, rh) = do_group_block_mining(
                        height,
                        block_intro_bin,
                        nonce_start,
                        current_nonce_space,
                    );
                    (hn, rh, 0u32, current_nonce_space)
                } else {
                    let workgroups_by_space = (current_nonce_space as u64 / unit_batch) as u32;
                    let workgroups_eff = workgroups_by_space.min(wg_cap);
                    let gpu_nonce_space = workgroups_eff
                        .saturating_mul(_cnf.localsize)
                        .saturating_mul(_cnf.unitsize);

                    if workgroups_eff == 0 {
                        let (hn, rh) = do_group_block_mining(
                            height,
                            block_intro_bin,
                            nonce_start,
                            current_nonce_space,
                        );
                        (hn, rh, 0u32, current_nonce_space)
                    } else {
                        match do_group_block_mining_opencl(
                            opencl,
                            height,
                            block_intro_bin.clone(),
                            nonce_start,
                            workgroups_eff,
                            _cnf.localsize,
                            _cnf.unitsize,
                        ) {
                            Err(e) => {
                                eprintln!("[efficiency] GPU batch failed: {}", e);
                                _cnf.runtime.record_gpu_error(
                                    wg_cap,
                                    _cnf.efficiency.oom_fallback,
                                );
                                let (hn, rh) = do_group_block_mining(
                                    height,
                                    block_intro_bin,
                                    nonce_start,
                                    current_nonce_space,
                                );
                                (hn, rh, 0u32, current_nonce_space)
                            }
                            Ok(mut best) => {
                                let tail_space =
                                    current_nonce_space.saturating_sub(gpu_nonce_space);
                                if tail_space > 0 {
                                    let tail_start = nonce_start.saturating_add(gpu_nonce_space);
                                    let cpu_tail = do_group_block_mining(
                                        height,
                                        block_intro_bin,
                                        tail_start,
                                        tail_space,
                                    );
                                    if hash_more_power(&cpu_tail.1, &best.1) {
                                        best = cpu_tail;
                                    }
                                }
                                (best.0, best.1, gpu_nonce_space, tail_space)
                            }
                        }
                    }
                }
            }
        };

        let use_secs = Instant::now().duration_since(ctn).as_millis() as f64 / 1000.0;
        let mlres = BlockMiningResult {
            height,
            nonce_start,
            nonce_space: current_nonce_space,
            gpu_nonce_space: gpu_ns,
            cpu_nonce_space: cpu_ns,
            head_nonce,
            coinbase_nonce: coinbase_nonce.to_vec(),
            result_hash: result_hash.to_vec(),
            target_hash: stuff.target_hash.to_vec(),
            use_secs,
            is_gpu: is_gpu_backend,
        };
        result_ch_tx.send(mlres.into()).unwrap();

        if use_secs > 0.0 {
            nonce_space = (current_nonce_space as f64 * MINING_INTERVAL / use_secs) as u32;
            nonce_space = nonce_space.max(1);
        }

        let Some(nst) = nonce_start.checked_add(current_nonce_space) else {
            return;
        };
        nonce_start = nst;

        // check next height
        let check_hei = MINING_BLOCK_HEIGHT.load(Relaxed);
        if check_hei > mining_hei {
            return; // turn to next height
        }
        // continue nonce space
    }
}

// return: nonce, hash
fn do_group_block_mining(
    height: u64,
    mut block_intro: Vec<u8>,
    nonce_start: u32,
    nonce_space: u32,
) -> (u32, [u8; 32]) {
    let mut most_nonce = 0u32;
    let mut most_hash = [255u8; 32];
    let nonce_end = nonce_start.checked_add(nonce_space).unwrap_or(u32::MAX);
    for nonce in nonce_start..nonce_end {
        // std::thread::sleep(std::time::Duration::from_millis(1)); // test
        block_intro[79..83].copy_from_slice(&nonce.to_be_bytes());
        let reshx = x16rs::block_hash(height, &block_intro);
        if hash_more_power(&reshx, &most_hash) {
            most_hash = reshx;
            most_nonce = nonce;
        }
    }
    // end
    (most_nonce, most_hash)
}

fn deal_block_mining_results(
    cnf: &PoWorkConf,
    most_hash: &mut Vec<u8>,
    result_ch_rx: &mut mpsc::Receiver<Arc<BlockMiningResult>>,
    worker_qty: usize,
) {
    let vene = worker_qty.max(1) as u32;
    // deal
    let mut deal_hei = 0u64;
    let mut most = Arc::new(BlockMiningResult::new());
    let mut total_nonce_space = 0u64;
    let mut gpu_nonce_space = 0u64;
    let mut cpu_nonce_space = 0u64;
    let mut total_use_secs = 0.0;
    let mut recv_count = 0;
    while let Ok(res) = result_ch_rx.try_recv() {
        deal_hei = res.height;
        total_nonce_space += res.nonce_space as u64;
        if res.gpu_nonce_space > 0 || res.cpu_nonce_space > 0 {
            gpu_nonce_space += res.gpu_nonce_space as u64;
            cpu_nonce_space += res.cpu_nonce_space as u64;
        } else if res.is_gpu {
            gpu_nonce_space += res.nonce_space as u64;
        } else {
            cpu_nonce_space += res.nonce_space as u64;
        }
        total_use_secs += res.use_secs; // Accumulated total time
        if hash_more_power(&res.result_hash, &most.result_hash) {
            most = res.clone();
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
    if hash_more_power(&most.result_hash, most_hash) {
        *most_hash = most.result_hash.clone();
    }
    // print hashrate
    let tarhx: [u8; HASH_WIDTH] = most.target_hash.clone().try_into().unwrap();
    let target_rates = hash_to_rates(&tarhx, TARGET_BLOCK_TIME);
    let avg_use_secs = total_use_secs / recv_count as f64;
    let nonce_rates = if avg_use_secs.is_finite() && avg_use_secs > 0.0 {
        total_nonce_space as f64 / avg_use_secs
    } else {
        0.0
    };
    let mut mnper = if target_rates.is_finite() && target_rates > 0.0 {
        nonce_rates / target_rates
    } else {
        0.0
    };
    if !mnper.is_finite() || mnper < 0.0 {
        mnper = 0.0;
    } else if mnper > 1.0 {
        mnper = 1.0;
    }
    let hac1day = mnper * ONEDAY_BLOCK_NUM * block_reward_number(deal_hei) as f64;
    let active_cpu = cnf.runtime.active_cpu_assist.load(Relaxed);
    cnf.runtime.maybe_adjust_supervene(&cnf.efficiency, gpu_nonce_space, cpu_nonce_space);
    if should_pause_for_profit(&cnf.efficiency, hac1day, &cnf.gpu_profile, active_cpu) {
        cnf.runtime.paused_unprofitable.store(true, Relaxed);
        println!(
            "\n[efficiency] Mining paused — estimated cost exceeds HAC revenue. Set pause_if_unprofitable=false or lower power draw."
        );
    } else {
        cnf.runtime.paused_unprofitable.store(false, Relaxed);
    }
    let eff_line = format_efficiency_line(
        nonce_rates,
        hac1day,
        mnper * 100.0,
        &cnf.efficiency,
        &cnf.gpu_profile,
        active_cpu,
    );
    flush!(
        "{} {} | {} | best {}.        \r",
        most.nonce_start,
        total_nonce_space,
        eff_line,
        hex::encode(hash_left_zero_pad3(&most_hash))
    );
    let paused = cnf.runtime.paused_unprofitable.load(Relaxed);
    let stats = build_mining_stats(
        nonce_rates,
        hac1day,
        mnper * 100.0,
        &cnf.efficiency,
        &cnf.gpu_profile,
        active_cpu,
        deal_hei,
        paused,
    );
    write_mining_stats(&cnf.efficiency.stats_file, &stats);
    // check success
    if cnf.debug == 1 || hash_more_power(&most.result_hash, &most.target_hash) {
        push_block_mining_success(cnf, &most);
    }
    // print next height
    may_print_turn_to_nex_block_mining(deal_hei, Some(most_hash));
}

fn may_print_turn_to_nex_block_mining(curr_hei: u64, most_hash: Option<&mut Vec<u8>>) {
    let mining_hei = MINING_BLOCK_HEIGHT.load(Relaxed);
    if curr_hei >= mining_hei {
        return; // not turn
    }
    if let Some(most_hash) = most_hash {
        *most_hash = vec![255u8; 32]; // reset 
    }
    let stuff = MINING_BLOCK_STUFF.read().unwrap();
    let tarhx = hash_left_zero_pad3(&stuff.target_hash.as_bytes()).to_hex();

    println!(
        "\n[{}] req height {} target {} to mining ... ",
        &ctshow()[5..],
        mining_hei,
        tarhx
    );
}

fn set_pending_block_stuff(height: u64, res: serde_json::Value) {
    let jstr = |k: &str| res[k].as_str().unwrap_or("");
    let _jnum = |k: &str| res[k].as_u64().unwrap_or(0);
    // data
    // println!("{:?}", &res);
    let target_hash = Hash::from(
        hex::decode(jstr("target_hash"))
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let block_intro = BlockIntro::must(&hex::decode(jstr("block_intro")).unwrap());
    let coinbase_tx = TransactionCoinbase::must(&hex::decode(jstr("coinbase_body")).unwrap());
    let mut mkrl_list = Vec::new();
    if let JV::Array(ref lists) = res["mkrl_modify_list"] {
        for li in lists {
            mkrl_list.push(Hash::from(
                hex::decode(li.as_str().unwrap_or(""))
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ));
        }
    }
    // set pending stuff
    let new_stuff = BlockMiningStuff {
        height,
        target_hash,
        block_intro,
        coinbase_tx,
        mkrl_list,
    };
    *MINING_BLOCK_STUFF.write().unwrap() = new_stuff.into();
    MINING_BLOCK_HEIGHT.store(height, Relaxed);
}

///////////////////////////////

fn pull_pending_block_stuff(cnf: &PoWorkConf) {
    let curr_hei = MINING_BLOCK_HEIGHT.load(Relaxed);

    // query pending
    let urlapi_pending = format!(
        "http://{}/query/miner/pending?stuff=true&t={}",
        &cnf.rpcaddr,
        sys::curtimes()
    );
    let res = HTTP_CLIENT.get(&urlapi_pending).send();
    let Ok(repv) = res else {
        println!("Error: cannot get block data at {}\n", &urlapi_pending);
        delay_return!(30);
    };
    let Ok(jsdata) = repv.text() else {
        println!(
            "Error: cannot read block data body at {}\n",
            &urlapi_pending
        );
        delay_return!(10);
    };
    let Ok(res) = serde_json::from_str::<JV>(&jsdata) else {
        println!(
            "Error: invalid block data json at {} (body len {})\n",
            &urlapi_pending,
            jsdata.len()
        );
        delay_return!(10);
    };
    let jstr = |k| res[k].as_str().unwrap_or("");
    let jnum = |k| res[k].as_u64().unwrap_or(0);
    let JV::String(ref _blkhd) = res["block_intro"] else {
        println!("Error: get block stuff error: {}", jstr("err"));
        delay_return!(15);
    };
    let pending_height = jnum("height");

    // set pending block stuff
    if pending_height > curr_hei {
        set_pending_block_stuff(pending_height, res);
        if curr_hei == 0 {
            may_print_turn_to_nex_block_mining(curr_hei, None); // print first
        }
    }

    // with notice
    let mut rpid = vec![0].repeat(16);
    loop {
        getrandom::fill(&mut rpid).unwrap();
        let urlapi_notice = format!(
            "http://{}/query/miner/notice?wait={}&height={}&rqid={}",
            &cnf.rpcaddr,
            &cnf.noticewait,
            pending_height,
            &hex::encode(&rpid)
        );
        // println!("\n-------- {} -------- {}\n", &ctshow(), &urlapi_notice);
        let res = HTTP_CLIENT
            .get(&urlapi_notice)
            .timeout(Duration::from_secs(300))
            .send();
        let Ok(repv) = res else {
            println!("Error: cannot get miner notice at {}\n", &urlapi_notice);
            delay_return!(10);
        };
        let Ok(jsdata) = repv.text() else {
            println!("Error: cannot read miner notice at {}", &urlapi_notice);
            delay_return!(1);
        };
        let Ok(res2) = serde_json::from_str::<JV>(&jsdata) else {
            // println!("{}", &jsdata);
            panic!("miner notice error: {}", &jsdata);
        };
        let jnum = |k| res2[k].as_u64().unwrap_or(0);
        let res_hei = jnum("height");
        // println!("\n++++++++ {} {} {}\n", &jsdata, res_hei, current_height);
        if res_hei >= pending_height {
            // next block discover
            break;
        }
        // continue to wait
    }
}

fn push_block_mining_success(cnf: &PoWorkConf, success: &BlockMiningResult) {
    let urlapi_success = format!(
        "http://{}/submit/miner/success?height={}&block_nonce={}&coinbase_nonce={}&t={}",
        &cnf.rpcaddr,
        success.height,
        success.head_nonce,
        success.coinbase_nonce.to_hex(),
        sys::curtimes()
    );
    let res_text = match HTTP_CLIENT.get(&urlapi_success).send() {
        Ok(resp) => resp.text().unwrap_or_default(),
        Err(e) => format!("Request failed: {}", e),
    };
    println!("{} {}", &urlapi_success, res_text);
    // print
    println!(
        "\n\n████████████████ [MINING SUCCESS] Find a block height {},\n██ hash {} to submit.",
        success.height,
        success.result_hash.to_hex()
    );
    println!("▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔")
}

#[cfg(feature = "ocl")]
fn bench_block_hps(
    opencl: &OpenCLResources,
    cnf: &PoWorkConf,
    wg_eff: u32,
    us: u32,
    seconds: u64,
) -> f64 {
    let height = 1u64;
    let block_intro = BlockIntro::default().serialize();
    let batch = wg_eff.saturating_mul(cnf.localsize).saturating_mul(us);
    let deadline = Instant::now() + Duration::from_secs(seconds.max(3));
    let mut total_hashes = 0u64;
    let mut total_secs = 0.0f64;
    let mut nonce = 0u32;
    while Instant::now() < deadline {
        let ctn = Instant::now();
        if do_group_block_mining_opencl(
            opencl,
            height,
            block_intro.clone(),
            nonce,
            wg_eff,
            cnf.localsize,
            us,
        )
        .is_ok()
        {
            total_hashes += batch as u64;
        }
        let used = ctn.elapsed().as_secs_f64();
        if used > 0.0 {
            total_secs += used;
        }
        nonce = nonce.wrapping_add(batch);
    }
    if total_secs > 0.0 {
        total_hashes as f64 / total_secs
    } else {
        0.0
    }
}

fn run_block_mining_benchmark(cnf: &PoWorkConf, config_path: &str) {
    #[cfg(not(feature = "ocl"))]
    {
        let _ = (cnf, config_path);
        println!("[benchmark] Rebuild with --features ocl and use_opencl=true");
        return;
    }
    #[cfg(feature = "ocl")]
    {
        if !cnf.useopencl {
            println!("[benchmark] Set use_opencl=true in [gpu]");
            return;
        }
        let total_secs = cnf.efficiency.benchmark_seconds.max(15) as u64;
        let fine = cnf.efficiency.wants_fine_sweep();
        let profile_secs = if fine {
            (total_secs * 70 / 100).max(20)
        } else {
            total_secs
        };
        let sweep_secs = if fine {
            total_secs.saturating_sub(profile_secs).max(10)
        } else {
            0
        };

        let init_unitsize = if fine {
            cnf.unitsize.max(128)
        } else {
            cnf.unitsize
        };
        let opencl_resources = initialize_opencl(
            false,
            &cnf.opencldir,
            &cnf.platformid,
            &cnf.deviceids,
            &cnf.workgroups,
            &cnf.localsize,
            &init_unitsize,
        );
        if opencl_resources.is_empty() {
            println!("[benchmark] No OpenCL devices");
            return;
        }

        for (dev_i, opencl) in opencl_resources.iter().enumerate() {
            let profiles = benchmark_profiles_for_vendor(opencl.vendor);
            let per = (profile_secs / profiles.len() as u64).max(4);
            println!(
                "[benchmark] Device #{}: {}s x {} profiles{}",
                dev_i,
                per,
                profiles.len(),
                if fine { " + fine sweep" } else { "" }
            );

            let mut best_hps = 0.0f64;
            let mut best_profit = 0.0f64;
            let mut best_hps_profile = profiles[0];
            let mut best_profit_profile = profiles[0];

            for profile in profiles {
                let (wg, us) = profile_tuning(profile);
                let wg_eff = cnf.runtime.workgroups(opencl.workgroups.min(wg));
                let hps = bench_block_hps(opencl, cnf, wg_eff, us, per);
                let watts = cnf.efficiency.estimate_gpu_watts(profile);
                let kh_per_j = if watts > 0.0 {
                    hps / watts / 1000.0
                } else {
                    0.0
                };
                println!(
                    "[benchmark] dev{} {}: {} ({:.1} kH/J, wg={})",
                    dev_i,
                    profile,
                    rates_to_show(hps),
                    kh_per_j,
                    wg_eff
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

            let base_profile = match cnf.efficiency.mode {
                EfficiencyMode::Max => best_hps_profile,
                _ => best_profit_profile,
            };
            let (base_wg, us) = profile_tuning(base_profile);
            let mut pick = BenchmarkPick::from_profile(base_profile);

            if fine && sweep_secs > 0 {
                let vram = opencl.vram_bytes;
                let wg_sweep_secs = sweep_secs / 2;
                let us_sweep_secs = sweep_secs.saturating_sub(wg_sweep_secs).max(6);
                let candidates = sweep_workgroup_candidates(base_wg, vram, cnf.localsize, us);
                let per_wg = (wg_sweep_secs / candidates.len() as u64).max(3);
                let mut best_sweep_hps = 0.0f64;
                let mut best_sweep_wg = pick.workgroups;
                println!(
                    "[benchmark] dev{} fine wg sweep: {} candidates x {}s",
                    dev_i,
                    candidates.len(),
                    per_wg
                );
                for wg_try in candidates {
                    let wg_eff = cnf.runtime.workgroups(opencl.workgroups.min(wg_try));
                    let hps = bench_block_hps(opencl, cnf, wg_eff, us, per_wg);
                    println!(
                        "[benchmark] dev{} wg={}: {}",
                        dev_i,
                        wg_eff,
                        rates_to_show(hps)
                    );
                    if hps > best_sweep_hps {
                        best_sweep_hps = hps;
                        best_sweep_wg = wg_eff;
                    }
                }
                pick.workgroups = best_sweep_wg;

                let us_candidates =
                    sweep_unitsize_candidates(pick.unitsize, opencl.allocated_unitsize);
                let per_us = (us_sweep_secs / us_candidates.len() as u64).max(3);
                let mut best_us_hps = 0.0f64;
                let mut best_us = pick.unitsize;
                println!(
                    "[benchmark] dev{} fine unit_size sweep: {:?} x {}s",
                    dev_i,
                    us_candidates,
                    per_us
                );
                for us_try in us_candidates {
                    let hps = bench_block_hps(opencl, cnf, pick.workgroups, us_try, per_us);
                    println!(
                        "[benchmark] dev{} unit_size={}: {}",
                        dev_i,
                        us_try,
                        rates_to_show(hps)
                    );
                    if hps > best_us_hps {
                        best_us_hps = hps;
                        best_us = us_try;
                    }
                }
                pick.unitsize = best_us;
            }

            println!(
                "[benchmark] dev{} pick: profile={} work_groups={} unit_size={} (mode={})",
                dev_i,
                pick.profile,
                pick.workgroups,
                pick.unitsize,
                cnf.efficiency.mode.label()
            );

            if dev_i == 0 {
                match apply_benchmark_pick(config_path, &pick) {
                    Ok(()) => {
                        println!("[benchmark] Config updated — restart mining with the new tuning.")
                    }
                    Err(e) => println!("[benchmark] Could not patch ini: {}", e),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_group_mining_result_matches_manual_scan() {
        let height = 1u64;
        let block_intro = BlockIntro::default().serialize();
        let nonce_start = 11u32;
        let nonce_space = 256u32;

        let (best_nonce, best_hash) =
            do_group_block_mining(height, block_intro.clone(), nonce_start, nonce_space);

        let mut manual_nonce = 0u32;
        let mut manual_hash = [255u8; 32];
        let mut intro = block_intro;
        for nonce in nonce_start..nonce_start + nonce_space {
            intro[79..83].copy_from_slice(&nonce.to_be_bytes());
            let hx = x16rs::block_hash(height, &intro);
            if hash_more_power(&hx, &manual_hash) {
                manual_hash = hx;
                manual_nonce = nonce;
            }
        }

        assert_eq!(best_nonce, manual_nonce);
        assert_eq!(best_hash, manual_hash);
    }
}
