use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::{Arc, RwLock, mpsc};

use std::thread::*;
use std::time::*;

use reqwest::blocking::Client as HttpClient;
use serde_json::Value as JV;

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
    Cpu,
    #[cfg(feature = "ocl")]
    Opencl(Arc<OpenCLResources>),
}

/*****************************************/

#[derive(Clone)]
pub struct PoWorkConf {
    pub rpcaddr: String,
    pub supervene: u32, // cpu core
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
}

impl PoWorkConf {
    pub fn new(ini: &IniObj) -> PoWorkConf {
        let sec = &ini_section(ini, "default"); // default = root
        let sec_gpu = &ini_section(ini, "gpu");
        let (workgroups, unitsize) = resolve_gpu_tuning(sec_gpu);
        let cnf = PoWorkConf {
            rpcaddr: ini_must(sec, "connect", "127.0.0.1:8081"),
            supervene: ini_must_u64(sec, "supervene", 2) as u32,
            noncemax: ini_must_u64(sec, "nonce_max", u32::MAX as u64) as u32,
            noticewait: ini_must_u64(sec, "notice_wait", 45),
            useopencl: ini_must_bool(sec_gpu, "use_opencl", false) as bool,
            workgroups,
            localsize: ini_must_u64(sec_gpu, "local_size", 256) as u32,
            unitsize,
            opencldir: ini_must(sec_gpu, "opencl_dir", "opencl/"),
            debug: ini_must_u64(sec_gpu, "debug", 0) as u32,
            platformid: ini_must_u64(sec_gpu, "platform_id", 0) as u32,
            deviceids: ini_must(sec_gpu, "device_ids", ""),
            cpu_assist: ini_must_bool(sec_gpu, "cpu_assist", true) as bool,
        };
        if !ini_must(sec_gpu, "gpu_profile", "").is_empty() {
            println!(
                "[gpu] profile={} work_groups={} unit_size={}",
                ini_must(sec_gpu, "gpu_profile", ""),
                cnf.workgroups,
                cnf.unitsize
            );
        }
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
    head_nonce: u32,
    coinbase_nonce: Vec<u8>,
    result_hash: Vec<u8>,
    target_hash: Vec<u8>,
    use_secs: f64,
}

impl BlockMiningResult {
    fn new() -> BlockMiningResult {
        let mut res = BlockMiningResult::default();
        res.result_hash = vec![255u8; 32];
        res
    }
}

pub fn poworker() {
    // config
    let cnfp = "./poworker.config.ini".to_string();
    let inicnf = sys::load_config(cnfp);
    let cnf = PoWorkConf::new(&inicnf);
    poworker_with_conf(cnf);
}

pub fn poworker_with_conf(cnf: PoWorkConf) {
    poworker_with_stop(cnf, None);
}

pub fn poworker_with_stop(cnf: PoWorkConf, stop_flag: Option<Arc<AtomicBool>>) {
    // test start
    // cnfobj.supervene = 1;
    // cnfobj.noncemax = u32::MAX / 200;
    // cnfobj.noticewait = 5;
    // test end

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
            let thrnum = cnf.supervene as usize;
            println!(
                "\n[Start] Create #{} Ryzen CPU assist threads (hybrid GPU+CPU).",
                thrnum
            );
            for _ in 0..thrnum {
                backends.push(MinerBackend::Cpu);
            }
        }
    }

    if backends.is_empty() {
        let thrnum = cnf.supervene.max(1) as usize;
        println!(
            "\n[Start] Create #{} CPU block miner worker thread.",
            thrnum
        );
        for _ in 0..thrnum {
            backends.push(MinerBackend::Cpu);
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
    let mut nonce_start: u32 = 0;
    let nonce_limit = _cnf.noncemax.max(1);
    let mut nonce_space: u32 = match backend {
        MinerBackend::Cpu => 100000,
        #[cfg(feature = "ocl")]
        MinerBackend::Opencl(_) => _cnf.workgroups * _cnf.localsize * _cnf.unitsize,
    };
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

        let (head_nonce, result_hash) = match &backend {
            MinerBackend::Cpu => {
                do_group_block_mining(height, block_intro_bin, nonce_start, current_nonce_space)
            }
            #[cfg(feature = "ocl")]
            MinerBackend::Opencl(opencl) => {
                let unit_batch = (_cnf.localsize as u64) * (_cnf.unitsize as u64);
                if _cnf.workgroups == 0 || unit_batch == 0 {
                    do_group_block_mining(height, block_intro_bin, nonce_start, current_nonce_space)
                } else {
                    let workgroups_by_space = (current_nonce_space as u64 / unit_batch) as u32;
                    let workgroups_eff = workgroups_by_space.min(_cnf.workgroups);
                    let gpu_nonce_space = workgroups_eff
                        .saturating_mul(_cnf.localsize)
                        .saturating_mul(_cnf.unitsize);

                    let mut best = if workgroups_eff > 0 {
                        do_group_block_mining_opencl(
                            opencl,
                            height,
                            block_intro_bin.clone(),
                            nonce_start,
                            workgroups_eff,
                            _cnf.localsize,
                            _cnf.unitsize,
                        )
                    } else {
                        (0u32, [255u8; 32])
                    };

                    let tail_space = current_nonce_space.saturating_sub(gpu_nonce_space);
                    if tail_space > 0 {
                        let tail_start = nonce_start.saturating_add(gpu_nonce_space);
                        let cpu_tail =
                            do_group_block_mining(height, block_intro_bin, tail_start, tail_space);
                        if hash_more_power(&cpu_tail.1, &best.1) {
                            best = cpu_tail;
                        }
                    }

                    best
                }
            }
        };

        let use_secs = Instant::now().duration_since(ctn).as_millis() as f64 / 1000.0;
        // record result
        let mlres = BlockMiningResult {
            height,
            nonce_start,
            nonce_space: current_nonce_space,
            head_nonce,
            coinbase_nonce: coinbase_nonce.to_vec(),
            result_hash: result_hash.to_vec(),
            target_hash: stuff.target_hash.to_vec(),
            use_secs,
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
    let mut total_use_secs = 0.0;
    let mut recv_count = 0;
    while let Ok(res) = result_ch_rx.try_recv() {
        deal_hei = res.height;
        total_nonce_space += res.nonce_space as u64;
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
    flush!(
        "{} {}, {} {}, ≈{:.4}HAC/day {:.6}%, {}.        \r",
        most.nonce_start,
        total_nonce_space,
        hex::encode(hash_left_zero_pad(&most.result_hash, 2)),
        hex::encode(hash_left_zero_pad3(&most_hash)),
        hac1day,
        mnper * 100.0,
        rates_to_show(nonce_rates)
    );
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
