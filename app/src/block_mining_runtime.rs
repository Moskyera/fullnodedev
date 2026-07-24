//! Block PoW miner backends, batch dispatch, and result aggregation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, RwLock, mpsc};

use std::thread::spawn;
use std::time::*;

use serde_json::Value as JV;

use crate::efficiency::*;
use crate::hash_util::{hash_left_zero_pad3, hash_more_power};
// The panic firewall is shared with the diamond (HACD) worker, which has exactly
// the same "one result thread owns every submission" shape.
use crate::mining_guard::guard_mining_iteration;
#[cfg(feature = "cuda")]
use crate::mining_batch::CudaBlockBackend;
#[cfg(feature = "ocl")]
use crate::mining_batch::OpenclBlockBackend;
use crate::mining_batch::{BatchCtx, BlockMinerBackend, CpuBlockBackend};

use basis::difficulty::*;
use basis::interface::*;
use field::*;
use mint::TransactionCoinbase;
use mint::genesis::block_reward_number;
use protocol::block::*;
use sys::*;

use super::PoWorkConf;

#[cfg(feature = "cuda")]
use super::CudaMiningResources;
#[cfg(feature = "ocl")]
use crate::opencl_gpu::{OpenclGpuHandle, initialize_opencl, opencl_snapshot_from_resource};

const HASH_WIDTH: usize = 32;
/// A real difficulty target never comes anywhere near this many leading zero
/// bytes (that would leave under 2^32 of the hash space). Refuse such a template
/// instead of installing an unmineable target from a hostile or buggy upstream.
const MAX_TARGET_LEADING_ZERO_BYTES: usize = 28;
/// Bounded result channel: an unbounded queue grows without limit whenever the
/// drain thread stalls. Under backpressure a statistics-only batch may be
/// dropped, but a result meeting its target is real money and always waits.
const RESULT_CHANNEL_CAPACITY: usize = 1024;
/// Winning results queued for the dedicated submit thread. Deep enough that a
/// burst of pool shares never blocks the result drain.
const SUBMIT_QUEUE_CAPACITY: usize = 256;
const MINING_INTERVAL: f64 = 3.0;
const WORKER_RATE_STALE_MS: u64 = 15_000;
const HASHRATE_EWMA_NEW_WEIGHT: f64 = 0.25;
const TARGET_BLOCK_TIME: f64 = 300.0;
const ONEDAY_BLOCK_NUM: f64 = 288.0;

static MINING_BLOCK_HEIGHT: AtomicU64 = AtomicU64::new(0);
/// Set while the upstream (pool bridge or fullnode) tells us the work it is
/// serving is no longer being refreshed. The installed template can no longer win
/// anything, so the workers idle instead of burning power on dead work.
static UPSTREAM_STALE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Bumped every time the template actually changes, INCLUDING a same-height reorg
/// (a new block replacing the tip at the same height). Workers watch this in
/// addition to the height so they stop grinding an orphaned template promptly.
static MINING_BLOCK_EPOCH: AtomicU64 = AtomicU64::new(0);
static MINING_BLOCK_STUFF: LazyLock<RwLock<Arc<BlockMiningStuff>>> =
    LazyLock::new(|| RwLock::default());

#[derive(Clone)]
pub(crate) enum MinerBackend {
    Cpu {
        assist_idx: Option<u32>,
    },
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
    worker_id: usize,
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

#[derive(Clone, Copy, Debug, Default)]
struct WorkerHashrate {
    gpu_hps: f64,
    cpu_hps: f64,
    updated_ms: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct HashrateTotals {
    gpu_hps: f64,
    cpu_hps: f64,
}

impl HashrateTotals {
    fn total_hps(self) -> f64 {
        self.gpu_hps + self.cpu_hps
    }
}

#[derive(Default)]
struct HashrateTracker {
    by_worker: HashMap<usize, WorkerHashrate>,
}

impl HashrateTracker {
    fn record_result(&mut self, result: &BlockMiningResult, now_ms: u64) {
        self.record_sample(
            result.worker_id,
            result.nonce_space,
            result.gpu_nonce_space,
            result.cpu_nonce_space,
            result.is_gpu,
            result.use_secs,
            now_ms,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn record_sample(
        &mut self,
        worker_id: usize,
        nonce_space: u32,
        gpu_nonce_space: u32,
        cpu_nonce_space: u32,
        is_gpu: bool,
        use_secs: f64,
        now_ms: u64,
    ) {
        if !use_secs.is_finite() || use_secs <= 0.0 {
            return;
        }
        let mut gpu_nonce = gpu_nonce_space as u64;
        let mut cpu_nonce = cpu_nonce_space as u64;
        if gpu_nonce == 0 && cpu_nonce == 0 {
            if is_gpu {
                gpu_nonce = nonce_space as u64;
            } else {
                cpu_nonce = nonce_space as u64;
            }
        }
        let sample_gpu = gpu_nonce as f64 / use_secs;
        let sample_cpu = cpu_nonce as f64 / use_secs;
        self.by_worker
            .entry(worker_id)
            .and_modify(|rate| {
                rate.gpu_hps += (sample_gpu - rate.gpu_hps) * HASHRATE_EWMA_NEW_WEIGHT;
                rate.cpu_hps += (sample_cpu - rate.cpu_hps) * HASHRATE_EWMA_NEW_WEIGHT;
                rate.updated_ms = now_ms;
            })
            .or_insert(WorkerHashrate {
                gpu_hps: sample_gpu,
                cpu_hps: sample_cpu,
                updated_ms: now_ms,
            });
    }

    fn totals(&mut self, now_ms: u64) -> HashrateTotals {
        self.by_worker
            .retain(|_, rate| now_ms.saturating_sub(rate.updated_ms) <= WORKER_RATE_STALE_MS);
        self.by_worker
            .values()
            .fold(HashrateTotals::default(), |mut totals, rate| {
                totals.gpu_hps += rate.gpu_hps;
                totals.cpu_hps += rate.cpu_hps;
                totals
            })
    }
}

/// True when this result independently meets its own PoW target. The node
/// accepts a block whose hash <= target (equal-inclusive), so use the same
/// equal-inclusive test rather than the strict "more power" comparison, which
/// would drop a hash landing exactly on target.
fn result_meets_target(res: &BlockMiningResult) -> bool {
    !res.target_hash.is_empty() && !hash_more_power(&res.target_hash, &res.result_hash)
}

/// Keep EVERY distinct winning result. Against a pool each submission is an
/// independent PPLNS share keyed by (height, coinbase_nonce, block_nonce), so
/// collapsing winners by height alone would throw away earned shares (real
/// money). Only an exact repeat of that triple is a true duplicate.
fn record_winner(winners: &mut Vec<Arc<BlockMiningResult>>, res: &Arc<BlockMiningResult>) {
    let already_queued = winners.iter().any(|w| {
        w.height == res.height
            && w.head_nonce == res.head_nonce
            && w.coinbase_nonce == res.coinbase_nonce
    });
    if !already_queued {
        winners.push(res.clone());
    }
}

/// Hand a batch result to the drain thread. Returns false only when the drain
/// side is gone, so the worker knows to exit. Under backpressure a
/// statistics-only result is dropped with a log, but a result meeting its own
/// target is a payout and waits for space instead.
fn send_mining_result(
    result_ch_tx: &mpsc::SyncSender<Arc<BlockMiningResult>>,
    res: Arc<BlockMiningResult>,
) -> bool {
    match result_ch_tx.try_send(res) {
        Ok(()) => true,
        Err(mpsc::TrySendError::Full(res)) => {
            if result_meets_target(&res) {
                return result_ch_tx.send(res).is_ok();
            }
            eprintln!(
                "[Mining] Result queue full, dropped a statistics-only batch at height {}.",
                res.height
            );
            true
        }
        Err(mpsc::TrySendError::Disconnected(_)) => false,
    }
}

/// Queue a winning result for the submit thread. If the queue is full or the
/// thread is gone we submit inline rather than drop it: a dropped winner is
/// lost money.
fn queue_block_mining_success(
    cnf: &PoWorkConf,
    submit_tx: &mpsc::SyncSender<Arc<BlockMiningResult>>,
    win: &Arc<BlockMiningResult>,
) {
    match submit_tx.try_send(win.clone()) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(win)) => {
            eprintln!(
                "[Mining] Submit queue full, submitting height {} inline.",
                win.height
            );
            super::push_block_mining_success(cnf, &win);
        }
        Err(mpsc::TrySendError::Disconnected(win)) => {
            super::push_block_mining_success(cnf, &win);
        }
    }
}

pub(crate) fn start_block_mining_workers(
    cnf: &PoWorkConf,
    stop_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> bool {
    let (res_tx, res_rx) = mpsc::sync_channel(RESULT_CHANNEL_CAPACITY);
    let miner_backends = build_miner_backends(cnf);
    if miner_backends.is_empty() {
        return false;
    }

    let thermal_devices: Vec<_> = miner_backends
        .iter()
        .filter_map(|backend| match backend {
            #[cfg(feature = "ocl")]
            MinerBackend::Opencl(gpu) => Some(gpu.sensor_identity()),
            #[cfg(feature = "cuda")]
            MinerBackend::Cuda(_) => Some((
                crate::gpu_arch::GpuVendor::Nvidia,
                cnf.cudadevice.max(0) as u32,
            )),
            MinerBackend::Cpu { .. } => None,
        })
        .collect();
    if !thermal_devices.is_empty() {
        crate::mining_runtime::start_thermal_monitor(
            &cnf.runtime,
            &cnf.efficiency,
            cnf.workgroups,
            &thermal_devices,
            stop_flag.clone(),
        );
    }

    // Submitting a winner is a blocking HTTP round-trip (with retries), so it runs
    // on a dedicated thread: doing it inline would stall the 123ms result drain and
    // back the result channel up whenever a pool serves an easy share target.
    let (submit_tx, submit_rx) =
        mpsc::sync_channel::<Arc<BlockMiningResult>>(SUBMIT_QUEUE_CAPACITY);
    let cnf_submit = cnf.clone();
    let submit_thread_guard = cnf.runtime.track_mining_thread();
    spawn(move || {
        let _submit_thread_guard = submit_thread_guard;
        while let Ok(win) = submit_rx.recv() {
            guard_mining_iteration("block submit thread", || {
                super::push_block_mining_success(&cnf_submit, &win);
            });
        }
    });

    let cnf1 = cnf.clone();
    let worker_qty = miner_backends.len();
    let stop_flag_res = stop_flag.clone();
    let result_thread_guard = cnf.runtime.track_mining_thread();
    spawn(move || {
        // submit_tx lives in this thread: when the drain loop returns on shutdown it
        // drops, which is what tells the submit thread to finish and exit.
        let _result_thread_guard = result_thread_guard;
        let rate_clock = Instant::now();
        let mut rate_tracker = HashrateTracker::default();
        let mut most_hash = vec![255u8; 32];
        let mut rstx = res_rx;
        loop {
            if super::should_stop(&stop_flag_res) {
                return;
            }
            let now_ms = rate_clock.elapsed().as_millis() as u64;
            // A panic here must never end the thread: this is the only path that
            // submits winning blocks, so losing it means silent total payout loss.
            guard_mining_iteration("block result thread", || {
                deal_block_mining_results(
                    &cnf1,
                    &mut most_hash,
                    &mut rstx,
                    worker_qty,
                    &mut rate_tracker,
                    now_ms,
                    &submit_tx,
                );
            });
            std::thread::sleep(Duration::from_millis(123));
        }
    });

    for (thrid, backend) in miner_backends.into_iter().enumerate() {
        let cnf2 = cnf.clone();
        let rstx = res_tx.clone();
        let stop_flag_miner = stop_flag.clone();
        let mining_thread_guard = cnf.runtime.track_mining_thread();
        spawn(move || {
            let _mining_thread_guard = mining_thread_guard;
            loop {
                if super::should_stop(&stop_flag_miner) {
                    return;
                }
                guard_mining_iteration("block mining worker", || {
                    run_block_mining_item(
                        &cnf2,
                        thrid,
                        rstx.clone(),
                        backend.clone(),
                        &stop_flag_miner,
                    );
                });
                std::thread::sleep(Duration::from_millis(9));
            }
        });
    }
    true
}

pub(crate) fn current_mining_height() -> u64 {
    MINING_BLOCK_HEIGHT.load(Relaxed)
}

pub(crate) fn set_pending_block_stuff(height: u64, res: JV) -> Result<(), String> {
    let decode = |key: &str| -> Result<Vec<u8>, String> {
        let value = res[key].as_str().ok_or_else(|| format!("missing {key}"))?;
        hex::decode(value).map_err(|e| format!("invalid {key}: {e}"))
    };

    let target_bytes = decode("target_hash")?;
    let target_array: [u8; HASH_WIDTH] = target_bytes
        .try_into()
        .map_err(|v: Vec<u8>| format!("invalid target_hash length: {}", v.len()))?;
    // Defense in depth: an upstream (pool or node) that hands us an absurd target
    // would install an unmineable template and drive the display math into
    // pathological ranges. Reject it so the pull loop simply retries.
    let leading_zero_bytes = target_array.iter().take_while(|byte| **byte == 0).count();
    if leading_zero_bytes >= MAX_TARGET_LEADING_ZERO_BYTES {
        return Err(format!(
            "implausible target_hash with {leading_zero_bytes} leading zero bytes"
        ));
    }
    let target_hash = Hash::from(target_array);

    let intro_bytes = decode("block_intro")?;
    let block_intro =
        BlockIntro::build(&intro_bytes).map_err(|e| format!("invalid block_intro: {e}"))?;
    let coinbase_bytes = decode("coinbase_body")?;
    let coinbase_tx = TransactionCoinbase::build(&coinbase_bytes)
        .map_err(|e| format!("invalid coinbase_body: {e}"))?;

    let mut mkrl_list = Vec::new();
    if let JV::Array(ref lists) = res["mkrl_modify_list"] {
        for li in lists {
            let text = li
                .as_str()
                .ok_or_else(|| "invalid mkrl_modify_list entry".to_string())?;
            let bytes =
                hex::decode(text).map_err(|e| format!("invalid mkrl_modify_list entry: {e}"))?;
            let array: [u8; HASH_WIDTH] = bytes
                .try_into()
                .map_err(|v: Vec<u8>| format!("invalid merkle hash length: {}", v.len()))?;
            mkrl_list.push(Hash::from(array));
        }
    }
    let new_stuff = BlockMiningStuff {
        height,
        target_hash,
        block_intro,
        coinbase_tx,
        mkrl_list,
    };
    let mut guard = MINING_BLOCK_STUFF
        .write()
        .map_err(|e| format!("mining state lock poisoned: {e}"))?;
    *guard = new_stuff.into();
    MINING_BLOCK_HEIGHT.store(height, Relaxed);
    MINING_BLOCK_EPOCH.fetch_add(1, Relaxed);
    Ok(())
}

pub(crate) fn current_mining_epoch() -> u64 {
    MINING_BLOCK_EPOCH.load(Relaxed)
}

/// Record whether the upstream is serving stale (no longer refreshed) work.
/// Returns true when the state actually changed, so the caller can log the
/// transition once instead of on every poll.
pub(crate) fn set_upstream_stale(stale: bool) -> bool {
    UPSTREAM_STALE.swap(stale, Relaxed) != stale
}

/// True while the installed template is known to be dead work.
pub(crate) fn upstream_is_stale() -> bool {
    UPSTREAM_STALE.load(Relaxed)
}

fn build_miner_backends(cnf: &PoWorkConf) -> Vec<MinerBackend> {
    let mut backends = Vec::new();

    if cnf.usecuda {
        #[cfg(feature = "cuda")]
        {
            let cuda_resources =
                super::initialize_cuda(cnf.cudadevice, cnf.workgroups, cnf.unitsize);
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
                    cnf.runtime.report_gpu_workgroups(
                        gpu.effective_wg(),
                        cnf.runtime.thermal_workgroups_cap(),
                        cnf.workgroups,
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
        if cnf.useopencl || cnf.usecuda {
            eprintln!(
                "[Fatal] a GPU miner was requested but no usable GPU backend initialized; refusing silent CPU fallback (you would pay for GPU power while mining slowly on the CPU). Check the driver/CUDA runtime, or set the backend to CPU."
            );
            return backends;
        }
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

fn backend_nonce_space(_cnf: &PoWorkConf, backend: &MinerBackend) -> u32 {
    match backend {
        MinerBackend::Cpu { .. } => 100_000,
        #[cfg(feature = "ocl")]
        MinerBackend::Opencl(gpu) => {
            let wg = gpu.workgroups(_cnf.workgroups, _cnf.runtime.thermal_workgroups_cap());
            wg.saturating_mul(_cnf.localsize)
                .saturating_mul(_cnf.unitsize)
                .max(1)
        }
        #[cfg(feature = "cuda")]
        MinerBackend::Cuda(res) => {
            // Match run_batch: the planned window must reflect the same effective
            // work-groups (OOM/error backoff) and thermal cap the batch will use,
            // otherwise the nonce accounting overstates what the GPU covered.
            let thermal = _cnf
                .runtime
                .thermal_workgroups_cap()
                .unwrap_or(u32::MAX);
            let wg = res
                .effective_wg()
                .min(_cnf.workgroups)
                .min(thermal)
                .max(1);
            wg.saturating_mul(x16rs_cuda::DEFAULT_LOCAL_SIZE)
                .saturating_mul(res.unit_size)
                .max(1)
        }
    }
}

fn next_nonce_space(current: u32, use_secs: f64, is_gpu_backend: bool) -> u32 {
    // A GPU backend can process exactly one effective device batch. Expanding
    // this window creates a CPU tail that hides the GPU for several seconds.
    if is_gpu_backend || !use_secs.is_finite() || use_secs <= 0.0 {
        return current.max(1);
    }
    ((current as f64 * MINING_INTERVAL / use_secs) as u32).max(1)
}

fn run_block_mining_item(
    _cnf: &PoWorkConf,
    thrid: usize,
    result_ch_tx: mpsc::SyncSender<Arc<BlockMiningResult>>,
    backend: MinerBackend,
    stop_flag: &Option<Arc<std::sync::atomic::AtomicBool>>,
) {
    if super::should_stop(stop_flag) {
        return;
    }
    if mining_is_gated(&_cnf.runtime, &_cnf.efficiency) {
        std::thread::sleep(Duration::from_millis(2000));
        return;
    }
    // The upstream told us it can no longer refresh this template. Hashing it
    // cannot win a block or earn a share, so idle until fresh work arrives
    // instead of paying for power that buys nothing.
    if upstream_is_stale() {
        std::thread::sleep(Duration::from_millis(1000));
        return;
    }

    let mining_hei = MINING_BLOCK_HEIGHT.load(Relaxed);
    let mining_epoch = current_mining_epoch();
    if mining_hei == 0 {
        std::thread::sleep(Duration::from_millis(111));
        return;
    }

    let mut coinbase_nonce = [0u8; HASH_WIDTH];
    if let Err(e) = getrandom::fill(&mut coinbase_nonce) {
        eprintln!("[Mining] Secure random nonce failed: {e}");
        return;
    }
    let coinbase_nonce = Hash::from(coinbase_nonce);
    if let MinerBackend::Cpu {
        assist_idx: Some(idx),
    } = &backend
    {
        let active = _cnf.runtime.active_cpu_assist.load(Relaxed);
        if *idx >= active {
            std::thread::sleep(Duration::from_millis(400));
            return;
        }
    }

    let mut nonce_start: u32 = 0;
    let nonce_limit = _cnf.noncemax.max(1);
    let mut nonce_space = backend_nonce_space(_cnf, &backend);
    let is_gpu_backend = match &backend {
        #[cfg(feature = "ocl")]
        MinerBackend::Opencl(_) => true,
        #[cfg(feature = "cuda")]
        MinerBackend::Cuda(_) => true,
        _ => false,
    };
    let stuff = match MINING_BLOCK_STUFF.read() {
        Ok(stuff) => stuff.clone(),
        Err(e) => {
            eprintln!("[Mining] Block state lock failed: {e}");
            return;
        }
    };
    let height = stuff.height;
    let mut coinbase_tx = stuff.coinbase_tx.clone();
    coinbase_tx.set_nonce(coinbase_nonce);
    let mut block_intro = stuff.block_intro.clone();
    block_intro.set_mrklroot(calculate_mrkl_prelude_update(
        coinbase_tx.hash(),
        &stuff.mkrl_list,
    ));
    // The header bytes are invariant across batches at this height (the kernel
    // varies only the nonce field), so serialize ONCE and clone per batch instead
    // of re-serializing every iteration.
    let block_intro_bin = block_intro.serialize();
    loop {
        if super::should_stop(stop_flag)
            || _cnf.runtime.thermal_pause_active()
            || upstream_is_stale()
        {
            return;
        }
        if nonce_start >= nonce_limit {
            return;
        }
        if is_gpu_backend {
            // Re-read the OOM/thermal-adjusted capacity before every GPU batch.
            nonce_space = backend_nonce_space(_cnf, &backend);
        }

        let remain = nonce_limit.saturating_sub(nonce_start);
        let current_nonce_space = nonce_space.min(remain).max(1);
        let ctn = Instant::now();

        let batch_ctx = BatchCtx {
            height,
            block_intro: block_intro_bin.clone(),
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

        let use_secs = ctn.elapsed().as_secs_f64();
        let mlres = BlockMiningResult {
            worker_id: thrid,
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
        if !send_mining_result(&result_ch_tx, mlres.into()) {
            return;
        }

        nonce_space = next_nonce_space(current_nonce_space, use_secs, is_gpu_backend);

        // Advance by the nonces actually mined, not the planned window. After a GPU
        // error/OOM the bounded recovery only covers a prefix of the window, so
        // skipping the whole planned span would leave large unmined nonce holes.
        // Advancing by the covered count keeps coverage contiguous (the next batch
        // re-tries the remainder). For a normal batch the covered count equals the
        // planned window, so this is a no-op there.
        let mined = gpu_ns.saturating_add(cpu_ns);
        let advance = if mined > 0 {
            mined.min(current_nonce_space)
        } else {
            current_nonce_space
        };
        let Some(nst) = nonce_start.checked_add(advance) else {
            return;
        };
        nonce_start = nst;

        let check_hei = MINING_BLOCK_HEIGHT.load(Relaxed);
        // Roll over on a height advance OR a same-height reorg (epoch bump): both
        // mean the template we are grinding is no longer the one to mine.
        if check_hei > mining_hei || current_mining_epoch() != mining_epoch {
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
    rate_tracker: &mut HashrateTracker,
    now_ms: u64,
    submit_tx: &mpsc::SyncSender<Arc<BlockMiningResult>>,
) {
    let vene = worker_qty.max(1) as u32;
    let mut deal_hei = 0u64;
    let mut most = Arc::new(BlockMiningResult::new());
    let mut total_nonce_space = 0u64;
    let mut gpu_nonce_space = 0u64;
    let mut cpu_nonce_space = 0u64;
    let mut recv_count = 0;
    // Every result that independently meets its own target is real money: a winning
    // block when solo, a creditable PPLNS share when pooled. Draining keeps only the
    // single strongest hash for stats, so collect ALL of them separately, otherwise
    // every winner but one in this drain window would be silently lost.
    let mut winners: Vec<Arc<BlockMiningResult>> = Vec::new();
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
        rate_tracker.record_result(&res, now_ms);
        if hash_more_power(&res.result_hash, &most.result_hash) {
            most = res.clone();
        }
        if result_meets_target(&res) {
            record_winner(&mut winners, &res);
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
    let Ok(tarhx) = most.target_hash.clone().try_into() else {
        eprintln!("[Mining] Ignoring result with invalid target hash length.");
        return;
    };
    let target_rates = hash_to_rates(&tarhx, TARGET_BLOCK_TIME);
    let rates = rate_tracker.totals(now_ms);
    let gpu_hashrate = rates.gpu_hps;
    let cpu_hashrate = rates.cpu_hps;
    let nonce_rates = rates.total_hps();
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
            "\n[efficiency] Mining paused: estimated cost exceeds HAC revenue. Set pause_if_unprofitable=false or lower power draw."
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
    if !winners.is_empty() {
        // Submit every distinct winner. Each is a payout we must not drop just
        // because another result in this window had a stronger hash: a pool credits
        // each (height, coinbase_nonce, block_nonce) as its own share.
        for w in &winners {
            queue_block_mining_success(cnf, submit_tx, w);
        }
    } else if cnf.debug == 1 {
        // Debug mode exercises the submit path even without a genuine winner.
        queue_block_mining_success(cnf, submit_tx, &most);
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
    let Ok(stuff) = MINING_BLOCK_STUFF.read() else {
        eprintln!("[Mining] Cannot read block state.");
        return;
    };
    let tarhx = hash_left_zero_pad3(&stuff.target_hash.as_bytes()).to_hex();

    println!(
        "\n[{}] req height {} target {} to mining ... ",
        &ctshow()[5..],
        mining_hei,
        tarhx
    );
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_pending_block_is_rejected_without_panicking() {
        assert!(set_pending_block_stuff(1, serde_json::json!({})).is_err());
        let invalid_hex = serde_json::json!({"target_hash": "zz"});
        assert!(set_pending_block_stuff(1, invalid_hex).is_err());
    }

    fn winner_for_test(
        height: u64,
        coinbase_nonce: u8,
        head_nonce: u32,
        hash: u8,
    ) -> Arc<BlockMiningResult> {
        let mut res = BlockMiningResult::default();
        res.height = height;
        res.head_nonce = head_nonce;
        res.coinbase_nonce = vec![coinbase_nonce; HASH_WIDTH];
        res.result_hash = vec![hash; HASH_WIDTH];
        res.target_hash = vec![0x0f; HASH_WIDTH];
        Arc::new(res)
    }

    #[test]
    fn every_distinct_share_at_one_height_is_kept_for_submission() {
        // A pool credits each (height, coinbase_nonce, block_nonce) separately, so
        // keeping only the strongest hash per height silently loses earned shares.
        let mut winners = Vec::new();
        let strong = winner_for_test(7, 1, 100, 0x01);
        let weak = winner_for_test(7, 2, 200, 0x0e);
        let same_worker_next_batch = winner_for_test(7, 1, 300, 0x05);
        record_winner(&mut winners, &strong);
        record_winner(&mut winners, &weak);
        record_winner(&mut winners, &same_worker_next_batch);
        assert_eq!(winners.len(), 3);

        // Only an exact repeat of the same triple is a true duplicate.
        record_winner(&mut winners, &winner_for_test(7, 2, 200, 0x0e));
        assert_eq!(winners.len(), 3);
    }

    #[test]
    fn a_hash_landing_exactly_on_target_still_counts_as_a_winner() {
        let mut res = BlockMiningResult::default();
        res.target_hash = vec![0x0f; HASH_WIDTH];
        res.result_hash = res.target_hash.clone();
        assert!(result_meets_target(&res));
        res.result_hash = vec![0x10; HASH_WIDTH];
        assert!(!result_meets_target(&res));
        res.target_hash = Vec::new();
        assert!(!result_meets_target(&res));
    }

    #[test]
    fn a_panicking_iteration_never_ends_the_mining_thread() {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut iterations = 0u32;
        for round in 0..3 {
            guard_mining_iteration("test loop", || {
                if round == 1 {
                    panic!("simulated result thread panic");
                }
            });
            iterations += 1;
        }
        std::panic::set_hook(previous_hook);
        assert_eq!(iterations, 3);
    }

    #[test]
    fn an_implausible_upstream_target_is_rejected() {
        let mut degenerate = [0u8; HASH_WIDTH];
        degenerate[31] = 1;
        let stuff = serde_json::json!({"target_hash": hex::encode(degenerate)});
        assert!(set_pending_block_stuff(1, stuff).is_err());
        let all_zero = serde_json::json!({"target_hash": hex::encode([0u8; HASH_WIDTH])});
        assert!(set_pending_block_stuff(1, all_zero).is_err());
    }

    #[test]
    fn a_full_result_queue_drops_statistics_but_never_a_winner() {
        let (tx, rx) = mpsc::sync_channel::<Arc<BlockMiningResult>>(1);
        let mut losing = BlockMiningResult::new();
        losing.target_hash = vec![0x0f; HASH_WIDTH];
        assert!(send_mining_result(&tx, Arc::new(losing.clone())));
        // Queue is full now: a statistics-only batch is dropped, not blocked.
        assert!(send_mining_result(&tx, Arc::new(losing)));
        assert_eq!(rx.try_recv().map(|r| r.height), Ok(0));

        let winner = winner_for_test(9, 1, 5, 0x01);
        assert!(send_mining_result(&tx, winner));
        assert_eq!(rx.try_recv().map(|r| r.height), Ok(9));
        drop(rx);
        assert!(!send_mining_result(&tx, winner_for_test(9, 1, 6, 0x01)));
    }

    #[test]
    fn stale_upstream_work_is_flagged_and_reported_once_per_transition() {
        assert!(!upstream_is_stale());
        assert!(set_upstream_stale(true));
        assert!(upstream_is_stale());
        // Repeating the same state is not a transition, so the operator gets one
        // message per outage instead of one per poll.
        assert!(!set_upstream_stale(true));
        assert!(set_upstream_stale(false));
        assert!(!upstream_is_stale());
    }

    #[test]
    fn gpu_nonce_window_never_expands_into_a_cpu_tail() {
        let gpu_window = 64 * 256 * 64;
        assert_eq!(next_nonce_space(gpu_window, 0.010, true), gpu_window);
        assert_eq!(next_nonce_space(gpu_window, 0.010, false), 314_572_800);
    }

    #[test]
    fn sequential_batches_from_one_worker_are_not_counted_as_parallel() {
        let mut tracker = HashrateTracker::default();
        for now_ms in [0, 10, 20, 30] {
            tracker.record_sample(0, 1_048_576, 1_048_576, 0, true, 0.010, now_ms);
        }
        let rates = tracker.totals(30);
        assert!((rates.gpu_hps - 104_857_600.0).abs() < 0.001);
        assert_eq!(rates.cpu_hps, 0.0);
        assert!((rates.total_hps() - rates.gpu_hps).abs() < f64::EPSILON);
    }

    #[test]
    fn latest_rates_from_parallel_workers_are_summed() {
        let mut tracker = HashrateTracker::default();
        tracker.record_sample(0, 1_000_000, 1_000_000, 0, true, 0.010, 100);
        tracker.record_sample(1, 100_000, 0, 100_000, false, 0.100, 100);
        tracker.record_sample(2, 100_000, 0, 100_000, false, 0.100, 100);

        let rates = tracker.totals(100);
        assert!((rates.gpu_hps - 100_000_000.0).abs() < 0.001);
        assert!((rates.cpu_hps - 2_000_000.0).abs() < 0.001);
        assert!((rates.total_hps() - 102_000_000.0).abs() < 0.001);
        assert!((rates.total_hps() - rates.gpu_hps - rates.cpu_hps).abs() < f64::EPSILON);
    }

    #[test]
    fn stale_worker_rates_expire() {
        let mut tracker = HashrateTracker::default();
        tracker.record_sample(0, 1_000_000, 1_000_000, 0, true, 0.010, 0);
        assert!(tracker.totals(WORKER_RATE_STALE_MS).total_hps() > 0.0);
        assert_eq!(tracker.totals(WORKER_RATE_STALE_MS + 1).total_hps(), 0.0);
    }
}
