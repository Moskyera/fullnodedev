//! Block PoW miner backends, batch dispatch, and result aggregation.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, RwLock, mpsc};

use std::thread::spawn;
use std::time::*;

use serde_json::Value as JV;

use crate::efficiency::*;
use crate::hash_util::{hash_left_zero_pad3, hash_more_power};
use crate::mining_batch::{BatchCtx, BlockMinerBackend, CpuBlockBackend};
#[cfg(feature = "ocl")]
use crate::mining_batch::OpenclBlockBackend;
#[cfg(feature = "cuda")]
use crate::mining_batch::CudaBlockBackend;

use basis::difficulty::*;
use basis::interface::*;
use field::*;
use mint::TransactionCoinbase;
use mint::genesis::block_reward_number;
use protocol::block::*;
use sys::*;

use super::PoWorkConf;

#[cfg(feature = "ocl")]
use crate::opencl_gpu::{
    initialize_opencl, opencl_snapshot_from_resource, OpenclGpuHandle,
};
#[cfg(feature = "cuda")]
use super::CudaMiningResources;

const HASH_WIDTH: usize = 32;
const MINING_INTERVAL: f64 = 3.0;
const TARGET_BLOCK_TIME: f64 = 300.0;
const ONEDAY_BLOCK_NUM: f64 = 288.0;

static MINING_BLOCK_HEIGHT: AtomicU64 = AtomicU64::new(0);
static MINING_BLOCK_STUFF: LazyLock<RwLock<Arc<BlockMiningStuff>>> =
    LazyLock::new(|| RwLock::default());

#[derive(Clone)]
pub(crate) enum MinerBackend {
    Cpu { assist_idx: Option<u32> },
    #[cfg(feature = "ocl")]
    Opencl(Arc<OpenclGpuHandle>),
    #[cfg(feature = "cuda")]
    Cuda(Arc<CudaMiningResources>),
}

#[derive(Clone, Default)]
struct BlockMiningStuff {
    height: u64,
    target_hash: Hash,
    block_intro: BlockIntro,
    coinbase_tx: TransactionCoinbase,
    mkrl_list: Vec<Hash>,
}

#[derive(Clone, Default)]
pub(crate) struct BlockMiningResult {
    pub height: u64,
    pub nonce_start: u32,
    nonce_space: u32,
    gpu_nonce_space: u32,
    cpu_nonce_space: u32,
    pub head_nonce: u32,
    pub coinbase_nonce: Vec<u8>,
    pub result_hash: Vec<u8>,
    pub target_hash: Vec<u8>,
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

pub(crate) fn start_block_mining_workers(
    cnf: &PoWorkConf,
    stop_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
) {
    let (res_tx, res_rx) = mpsc::channel();
    let miner_backends = build_miner_backends(cnf);

    let cnf1 = cnf.clone();
    let worker_qty = miner_backends.len();
    let stop_flag_res = stop_flag.clone();
    spawn(move || {
        let mut most_hash = vec![255u8; 32];
        let mut rstx = res_rx;
        loop {
            if super::should_stop(&stop_flag_res) {
                return;
            }
            deal_block_mining_results(&cnf1, &mut most_hash, &mut rstx, worker_qty);
            std::thread::sleep(Duration::from_millis(123));
        }
    });

    for (thrid, backend) in miner_backends.into_iter().enumerate() {
        let cnf2 = cnf.clone();
        let rstx = res_tx.clone();
        let stop_flag_miner = stop_flag.clone();
        spawn(move || {
            loop {
                if super::should_stop(&stop_flag_miner) {
                    return;
                }
                run_block_mining_item(&cnf2, thrid, rstx.clone(), backend.clone());
                std::thread::sleep(Duration::from_millis(9));
            }
        });
    }
}

pub(crate) fn current_mining_height() -> u64 {
    MINING_BLOCK_HEIGHT.load(Relaxed)
}

pub(crate) fn set_pending_block_stuff(height: u64, res: JV) {
    let jstr = |k: &str| res[k].as_str().unwrap_or("");
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

fn build_miner_backends(cnf: &PoWorkConf) -> Vec<MinerBackend> {
    let mut backends = Vec::new();

    if cnf.usecuda {
        #[cfg(feature = "cuda")]
        {
            let cuda_resources = super::initialize_cuda(cnf.cudadevice, cnf.workgroups, cnf.unitsize);
            if !cuda_resources.is_empty() {
                println!(
                    "\n[Start] Create CUDA block miner worker #{}.",
                    cuda_resources.len()
                );
                for resource in cuda_resources {
                    backends.push(MinerBackend::Cuda(resource));
                }
            }
        }
        #[cfg(not(feature = "cuda"))]
        {
            println!(
                "\n[Warn] use_cuda=true but app built without `cuda` feature, fallback to CPU miner."
            );
        }
    } else if cnf.useopencl {
        #[cfg(feature = "ocl")]
        {
            let scan = crate::opencl_diag::scan_opencl();
            let amd_icd_count = crate::opencl_diag::count_amd_platforms(&scan.platforms);
            let opencl_resources = initialize_opencl(
                false,
                &cnf.opencldir,
                &cnf.platformid,
                &cnf.deviceids,
                &cnf.workgroups,
                &cnf.localsize,
                &cnf.unitsize,
                Some(&scan),
                false,
            );
            if !opencl_resources.is_empty() {
                println!(
                    "\n[Start] Create GPU block miner worker #{}.",
                    opencl_resources.len()
                );
                for resource in opencl_resources {
                    let vram = resource.vram_bytes;
                    let arch = resource.arch_slug.clone();
                    let gpu_snapshot = opencl_snapshot_from_resource(
                        &resource,
                        false,
                        &cnf.opencldir,
                        cnf.localsize,
                        cnf.unitsize,
                        amd_icd_count,
                    );
                    let gpu = OpenclGpuHandle::new(resource, gpu_snapshot, scan.clone());
                    gpu.configure_oom_floor(
                        vram,
                        cnf.localsize,
                        cnf.unitsize,
                        cnf.workgroups,
                        &arch,
                    );
                    backends.push(MinerBackend::Opencl(gpu));
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
        std::thread::sleep(Duration::from_millis(2000));
        return;
    }

    let mining_hei = MINING_BLOCK_HEIGHT.load(Relaxed);
    if mining_hei == 0 {
        std::thread::sleep(Duration::from_millis(111));
        return;
    }

    let mut coinbase_nonce = [0u8; HASH_WIDTH];
    getrandom::fill(&mut coinbase_nonce).unwrap();
    let coinbase_nonce = Hash::from(coinbase_nonce);
    if let MinerBackend::Cpu { assist_idx: Some(idx) } = &backend {
        let active = _cnf.runtime.active_cpu_assist.load(Relaxed);
        if *idx >= active {
            std::thread::sleep(Duration::from_millis(400));
            return;
        }
    }

    let mut nonce_start: u32 = 0;
    let nonce_limit = _cnf.noncemax.max(1);
    let mut nonce_space: u32 = match &backend {
        MinerBackend::Cpu { .. } => 100000,
        #[cfg(feature = "ocl")]
        MinerBackend::Opencl(gpu) => {
            let wg = gpu.workgroups(_cnf.workgroups, _cnf.runtime.thermal_workgroups_cap());
            wg * _cnf.localsize * _cnf.unitsize
        }
        #[cfg(feature = "cuda")]
        MinerBackend::Cuda(res) => {
            let wg = res.workgroups.min(_cnf.workgroups);
            wg * x16rs_cuda::DEFAULT_LOCAL_SIZE * res.unit_size
        }
    };
    let is_gpu_backend = match &backend {
        #[cfg(feature = "ocl")]
        MinerBackend::Opencl(_) => true,
        #[cfg(feature = "cuda")]
        MinerBackend::Cuda(_) => true,
        _ => false,
    };
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

        let batch_ctx = BatchCtx {
            height,
            block_intro: block_intro_bin,
            nonce_start,
            nonce_space: current_nonce_space,
            configured_wg: _cnf.workgroups,
            localsize: _cnf.localsize,
            unitsize: _cnf.unitsize,
            thermal_wg_cap: _cnf.runtime.thermal_workgroups_cap(),
        };
        let cpu_mine = &do_group_block_mining;
        let batch = match &backend {
            MinerBackend::Cpu { .. } => CpuBlockBackend.run_batch(&batch_ctx, cpu_mine),
            #[cfg(feature = "cuda")]
            MinerBackend::Cuda(cuda) => {
                let b = CudaBlockBackend {
                    cuda: cuda.clone(),
                    configured_wg: _cnf.workgroups,
                    runtime: _cnf.runtime.clone(),
                };
                b.run_batch(&batch_ctx, cpu_mine)
            }
            #[cfg(feature = "ocl")]
            MinerBackend::Opencl(gpu) => {
                let b = OpenclBlockBackend {
                    gpu: gpu.clone(),
                    oom_fallback: _cnf.efficiency.oom_fallback,
                    runtime: _cnf.runtime.clone(),
                };
                b.run_batch(&batch_ctx, cpu_mine)
            }
        };
        let head_nonce = batch.head_nonce;
        let result_hash = batch.result_hash;
        let gpu_ns = batch.gpu_nonce_space;
        let cpu_ns = batch.cpu_nonce_space;

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

        let check_hei = MINING_BLOCK_HEIGHT.load(Relaxed);
        if check_hei > mining_hei {
            return;
        }
    }
}

pub(crate) fn do_group_block_mining(
    height: u64,
    mut block_intro: Vec<u8>,
    nonce_start: u32,
    nonce_space: u32,
) -> (u32, [u8; 32]) {
    let mut most_nonce = 0u32;
    let mut most_hash = [255u8; 32];
    let nonce_end = nonce_start.checked_add(nonce_space).unwrap_or(u32::MAX);
    for nonce in nonce_start..nonce_end {
        block_intro[79..83].copy_from_slice(&nonce.to_be_bytes());
        let reshx = x16rs::block_hash(height, &block_intro);
        if hash_more_power(&reshx, &most_hash) {
            most_hash = reshx;
            most_nonce = nonce;
        }
    }
    (most_nonce, most_hash)
}

fn deal_block_mining_results(
    cnf: &PoWorkConf,
    most_hash: &mut Vec<u8>,
    result_ch_rx: &mut mpsc::Receiver<Arc<BlockMiningResult>>,
    worker_qty: usize,
) {
    let vene = worker_qty.max(1) as u32;
    let mut deal_hei = 0u64;
    let mut most = Arc::new(BlockMiningResult::new());
    let mut total_nonce_space = 0u64;
    let mut gpu_nonce_space = 0u64;
    let mut cpu_nonce_space = 0u64;
    let mut gpu_batch_space = 0u64;
    let mut cpu_batch_space = 0u64;
    let mut gpu_batch_secs = 0.0f64;
    let mut cpu_batch_secs = 0.0f64;
    let mut total_use_secs = 0.0;
    let mut recv_count = 0;
    while let Ok(res) = result_ch_rx.try_recv() {
        deal_hei = res.height;
        total_nonce_space += res.nonce_space as u64;
        if res.gpu_nonce_space > 0 || res.cpu_nonce_space > 0 {
            gpu_nonce_space += res.gpu_nonce_space as u64;
            cpu_nonce_space += res.cpu_nonce_space as u64;
            if res.gpu_nonce_space > 0 {
                gpu_batch_space += res.gpu_nonce_space as u64;
                gpu_batch_secs += res.use_secs;
            }
            if res.cpu_nonce_space > 0 {
                cpu_batch_space += res.cpu_nonce_space as u64;
                cpu_batch_secs += res.use_secs;
            }
        } else if res.is_gpu {
            gpu_nonce_space += res.nonce_space as u64;
            gpu_batch_space += res.nonce_space as u64;
            gpu_batch_secs += res.use_secs;
        } else {
            cpu_nonce_space += res.nonce_space as u64;
            cpu_batch_space += res.nonce_space as u64;
            cpu_batch_secs += res.use_secs;
        }
        total_use_secs += res.use_secs;
        if hash_more_power(&res.result_hash, &most.result_hash) {
            most = res.clone();
        }
        recv_count += 1;
        if recv_count >= vene as usize * 4 {
            break;
        }
    }
    if recv_count == 0 {
        return;
    }
    if hash_more_power(&most.result_hash, most_hash) {
        *most_hash = most.result_hash.clone();
    }
    let tarhx: [u8; HASH_WIDTH] = most.target_hash.clone().try_into().unwrap();
    let target_rates = hash_to_rates(&tarhx, TARGET_BLOCK_TIME);
    let avg_use_secs = total_use_secs / recv_count as f64;
    let nonce_rates = if avg_use_secs.is_finite() && avg_use_secs > 0.0 {
        total_nonce_space as f64 / avg_use_secs
    } else {
        0.0
    };
    let mut gpu_hashrate = if gpu_batch_secs > 0.0 {
        gpu_batch_space as f64 / gpu_batch_secs
    } else {
        0.0
    };
    let cpu_hashrate = if cpu_batch_secs > 0.0 {
        cpu_batch_space as f64 / cpu_batch_secs
    } else {
        0.0
    };
    if gpu_hashrate <= 0.0 && nonce_rates > cpu_hashrate {
        gpu_hashrate = nonce_rates - cpu_hashrate;
    }
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
    cnf.runtime
        .maybe_adjust_supervene(&cnf.efficiency, gpu_nonce_space, cpu_nonce_space);
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
    crate::mining_stats::emit_from_batch_aggregate(
        &crate::mining_stats::BatchAggregate {
            hashrate: nonce_rates,
            hac_per_day: hac1day,
            network_pct: mnper * 100.0,
            height: deal_hei,
            gpu_hashrate,
            cpu_hashrate,
            paused,
        },
        &cnf.efficiency,
        &cnf.gpu_profile,
        active_cpu,
        cnf.workgroups,
        &cnf.runtime,
        "hac",
        0,
        "",
        &cnf.efficiency.stats_file,
    );
    if cnf.debug == 1 || hash_more_power(&most.result_hash, &most.target_hash) {
        super::push_block_mining_success(cnf, &most);
    }
    may_print_turn_to_nex_block_mining(deal_hei, Some(most_hash));
}

pub(crate) fn may_print_turn_to_nex_block_mining(curr_hei: u64, most_hash: Option<&mut Vec<u8>>) {
    let mining_hei = MINING_BLOCK_HEIGHT.load(Relaxed);
    if curr_hei >= mining_hei {
        return;
    }
    if let Some(most_hash) = most_hash {
        *most_hash = vec![255u8; 32];
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