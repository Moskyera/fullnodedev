use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::*};

use std::time::*;

use reqwest::blocking::Client as HttpClient;
use serde_json::Value as JV;

use crate::efficiency::*;

#[cfg(feature = "ocl")]
use basis::difficulty::*;
#[cfg(any(feature = "ocl", test))]
use field::*;
#[cfg(any(feature = "ocl", test))]
use protocol::block::*;
use sys::*;

#[cfg(feature = "ocl")]
use crate::opencl_gpu::block::do_group_block_mining_opencl;
#[cfg(feature = "ocl")]
use crate::opencl_gpu::initialize_opencl;
#[cfg(feature = "cuda")]
include! {"cuda_pow.rs"}

#[path = "block_mining_runtime.rs"]
mod block_mining_runtime;

/*****************************************/

#[derive(Clone)]
pub struct PoWorkConf {
    pub rpcaddr: String,
    /// Optional fullnode API token (`X-Api-Token`) when server requires auth.
    pub api_token: String,
    /// Optional payout address announced to a POOL as `&worker=<address>` so it
    /// can credit this miner's shares. Empty (default) = solo mining: the URLs
    /// stay byte-identical to what a plain fullnode expects.
    pub pool_worker: String,
    pub supervene: u32, // cpu core (configured)
    pub noncemax: u32,
    pub noticewait: u64,   // new block notice wait
    pub useopencl: bool,   // use opencl miner
    pub usecuda: bool,     // use cuda miner (NVIDIA)
    pub cudadevice: i32,   // cuda device index
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
    pub gpu_slug: String,
    pub efficiency: EfficiencyConf,
    pub runtime: Arc<MiningRuntimeState>,
}

/// Percent-encode a query-string component, leaving only the RFC 3986 unreserved
/// characters. HAC addresses are already safe, but this guards against a
/// misconfigured worker id breaking or injecting into the request URL.
fn percent_encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl PoWorkConf {
    /// `&worker=<payout address>` suffix appended to pool requests so the pool
    /// can credit shares to us. Empty string when solo mining. The value is
    /// percent-encoded so a misconfigured worker id cannot break or inject into
    /// the query string.
    pub fn worker_param(&self) -> String {
        if self.pool_worker.is_empty() {
            String::new()
        } else {
            format!("&worker={}", percent_encode_component(&self.pool_worker))
        }
    }

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
            api_token: ini_must(sec, "api_token", "").trim().to_string(),
            pool_worker: ini_must(sec, "pool_worker", "").trim().to_string(),
            supervene: configured_supervene,
            noncemax: ini_must_u64(sec, "nonce_max", u32::MAX as u64) as u32,
            noticewait: ini_must_u64(sec, "notice_wait", 45),
            useopencl: ini_must_bool(sec_gpu, "use_opencl", false) as bool,
            usecuda: ini_must_bool(sec_gpu, "use_cuda", false) as bool,
            cudadevice: ini_must_u64(sec_gpu, "cuda_device", 0) as i32,
            workgroups: tuning.workgroups,
            localsize: ini_must_u64(sec_gpu, "local_size", 256) as u32,
            unitsize: tuning.unitsize,
            opencldir: ini_must(sec_gpu, "opencl_dir", "opencl/"),
            debug: ini_must_u64(sec_gpu, "debug", 0) as u32,
            platformid: ini_must_u64(sec_gpu, "platform_id", 0) as u32,
            deviceids: ini_must(sec_gpu, "device_ids", ""),
            cpu_assist: ini_must_bool(sec_gpu, "cpu_assist", true) as bool,
            gpu_profile: tuning.profile.clone(),
            gpu_slug: ini_must(sec_gpu, "gpu_slug", ""),
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

use std::sync::LazyLock;
static HTTP_CLIENT: LazyLock<HttpClient> = LazyLock::new(|| {
    crate::rpc_http::build_client()
        .unwrap_or_else(|e| panic!("cannot create bounded RPC client: {e}"))
});

pub fn poworker() {
    let default_config = "./poworker.config.ini";
    let config_path = sys::resolve_config_path(default_config);
    let inicnf = sys::load_config_path(&config_path);
    let cnf = PoWorkConf::new(&inicnf);
    // Mainnet-representative (x16rs repeat=16) benchmark. Runs only when
    // HACASH_REPEAT16_BENCH_SECONDS is set to a positive integer, then exits.
    // Zero-touch when the variable is unset; see bench_mainnet_repeat16.rs.
    if crate::bench_mainnet_repeat16::run_from_env(
        &cnf.opencldir,
        &cnf.platformid,
        &cnf.deviceids,
        &cnf.workgroups,
        &cnf.localsize,
        &cnf.unitsize,
    ) {
        return;
    }
    if cnf.efficiency.benchmark_seconds > 0 {
        run_block_mining_benchmark(&cnf, config_path.to_string_lossy().as_ref());
        return;
    }
    poworker_with_conf(cnf);
}

pub fn poworker_with_conf(cnf: PoWorkConf) {
    poworker_with_stop(cnf, None);
}

pub fn poworker_with_stop(cnf: PoWorkConf, stop_flag: Option<Arc<AtomicBool>>) {
    if !block_mining_runtime::start_block_mining_workers(&cnf, stop_flag.clone()) {
        eprintln!("[Fatal] Mining worker startup failed.");
        return;
    }

    // loop
    loop {
        if should_stop(&stop_flag) {
            return;
        }
        if !is_within_idle_schedule(cnf.efficiency.idle_start_hour, cnf.efficiency.idle_end_hour) {
            delay_continue_ms!(5000);
        }
        if cnf.runtime.paused_unprofitable.load(Relaxed) {
            delay_continue_ms!(3000);
        }
        pull_pending_block_stuff(&cnf, &stop_flag);
        delay_continue_ms!(25);
    }
}

fn should_stop(stop_flag: &Option<Arc<AtomicBool>>) -> bool {
    stop_flag.as_ref().map(|f| f.load(Relaxed)).unwrap_or(false)
}

///////////////////////////////

/// Hex of the last block_intro we installed, so a same-height reorg (the tip
/// block replaced at the same height) is detected and picked up, not ignored.
static LAST_PENDING_INTRO: LazyLock<std::sync::Mutex<String>> =
    LazyLock::new(|| std::sync::Mutex::new(String::new()));

fn pull_pending_block_stuff(cnf: &PoWorkConf, stop_flag: &Option<Arc<AtomicBool>>) {
    let curr_hei = block_mining_runtime::current_mining_height();

    // query pending
    let urlapi_pending = format!(
        "http://{}/query/miner/pending?stuff=true&t={}{}",
        &cnf.rpcaddr,
        sys::curtimes(),
        cnf.worker_param()
    );
    let jsdata =
        match crate::rpc_http::get_text(&HTTP_CLIENT, &urlapi_pending, &cnf.api_token, None) {
            Ok(t) => t,
            Err(e) => {
                println!(
                    "Error: cannot get block data at {}: {}\n",
                    &urlapi_pending, e
                );
                delay_return!(30);
            }
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
    let new_intro = res["block_intro"].as_str().unwrap_or("").to_string();

    // Install on a height advance, OR a same-height reorg: if the node returns a
    // different block_intro at the SAME height, the tip was replaced and the old
    // template is now orphaned, so refresh instead of grinding dead work.
    let intro_changed = {
        let mut g = LAST_PENDING_INTRO.lock().unwrap_or_else(|e| e.into_inner());
        if *g != new_intro {
            *g = new_intro.clone();
            true
        } else {
            false
        }
    };
    if pending_height > curr_hei || (pending_height == curr_hei && intro_changed) {
        if let Err(e) = block_mining_runtime::set_pending_block_stuff(pending_height, res) {
            println!("Error: invalid block data from {urlapi_pending}: {e}");
            delay_return!(10);
        }
        if curr_hei == 0 {
            block_mining_runtime::may_print_turn_to_nex_block_mining(curr_hei, None);
        }
    }

    // with notice
    let mut rpid = vec![0].repeat(16);
    loop {
        // Exit promptly on shutdown instead of blocking up to the long-poll
        // timeout (~300s) inside the notice request below.
        if should_stop(stop_flag) {
            return;
        }
        if let Err(e) = getrandom::fill(&mut rpid) {
            println!("Error: cannot generate request id: {e}");
            delay_return!(1);
        }
        let urlapi_notice = format!(
            "http://{}/query/miner/notice?wait={}&height={}&rqid={}",
            &cnf.rpcaddr,
            &cnf.noticewait,
            pending_height,
            &hex::encode(&rpid)
        );
        // println!("\n-------- {} -------- {}\n", &ctshow(), &urlapi_notice);
        let jsdata = match crate::rpc_http::get_text(
            &HTTP_CLIENT,
            &urlapi_notice,
            &cnf.api_token,
            Some(Duration::from_secs(300)),
        ) {
            Ok(t) => t,
            Err(e) => {
                println!(
                    "Error: cannot get miner notice at {}: {}\n",
                    &urlapi_notice, e
                );
                delay_return!(10);
            }
        };
        let Ok(res2) = serde_json::from_str::<JV>(&jsdata) else {
            println!("Error: invalid miner notice JSON at {urlapi_notice}");
            delay_return!(1);
        };
        let jnum = |k| res2[k].as_u64().unwrap_or(0);
        let res_hei = jnum("height");
        // println!("\n++++++++ {} {} {}\n", &jsdata, res_hei, current_height);
        if res_hei >= pending_height {
            // next block discover
            break;
        }
        // No new block yet. A compliant server long-polls (so this rarely loops),
        // but a fast-returning/incompatible server or notice_wait=0 would otherwise
        // spin this at 100% CPU — cap the spin with a small delay before re-polling.
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn push_block_mining_success(cnf: &PoWorkConf, success: &block_mining_runtime::BlockMiningResult) {
    let urlapi_success = format!(
        "http://{}/submit/miner/success?height={}&block_nonce={}&coinbase_nonce={}&t={}{}",
        &cnf.rpcaddr,
        success.height,
        success.head_nonce,
        success.coinbase_nonce.to_hex(),
        sys::curtimes(),
        cnf.worker_param()
    );
    // Submitting the winning block is the entire payoff of solo mining, and the
    // result was already drained from the channel — so a single transient network
    // error must not silently lose it. Retry transport failures with backoff, and
    // only claim SUCCESS once the node confirms acceptance (ret == 0).
    const MAX_SUBMIT_ATTEMPTS: u32 = 5;
    let mut accepted = false;
    let mut last = String::new();
    for attempt in 1..=MAX_SUBMIT_ATTEMPTS {
        match crate::rpc_http::get_text(&HTTP_CLIENT, &urlapi_success, &cnf.api_token, None) {
            Ok(body) => {
                let parsed = serde_json::from_str::<JV>(&body).ok();
                let ret = parsed.as_ref().and_then(|j| j["ret"].as_i64());
                last = body.clone();
                match ret {
                    Some(0) => {
                        accepted = true;
                        break;
                    }
                    Some(_) => {
                        // Deterministic node rejection (stale height, invalid,
                        // etc.): retrying will not help, so stop and report it.
                        let err = parsed
                            .as_ref()
                            .and_then(|j| j["err"].as_str())
                            .unwrap_or("");
                        println!("[submit] node rejected height {}: {}", success.height, err);
                        break;
                    }
                    None => {
                        // HTTP 200 but no parseable `ret` (proxy/load-balancer error
                        // page, truncated or non-JSON body). This is NOT a node
                        // decision — treat it as transient and retry, so a winning
                        // block is not discarded on a front-end hiccup.
                        let snippet: String = body.chars().take(120).collect();
                        println!(
                            "[submit] attempt {}/{} unrecognized response, retrying: {}",
                            attempt, MAX_SUBMIT_ATTEMPTS, snippet
                        );
                        if attempt < MAX_SUBMIT_ATTEMPTS {
                            std::thread::sleep(Duration::from_millis(500u64 * attempt as u64));
                        }
                    }
                }
            }
            Err(e) => {
                last = format!("transport error: {e}");
                println!(
                    "[submit] attempt {}/{} failed: {e}",
                    attempt, MAX_SUBMIT_ATTEMPTS
                );
                if attempt < MAX_SUBMIT_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(500u64 * attempt as u64));
                }
            }
        }
    }
    println!("{} {}", &urlapi_success, last);
    if accepted {
        println!(
            "\n\n████████████████ [MINING SUCCESS] Find a block height {},\n██ hash {} to submit.",
            success.height,
            success.result_hash.to_hex()
        );
    } else {
        println!(
            "\n\n████████████████ [MINING SUBMIT FAILED] block height {} was NOT confirmed accepted\n██ after {} attempts (hash {}). Check the node/connection.",
            success.height,
            MAX_SUBMIT_ATTEMPTS,
            success.result_hash.to_hex()
        );
    }
    println!("▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔")
}
#[cfg(feature = "ocl")]
const AUTOTUNE_WARMUP_BATCHES: u32 = 3;
#[cfg(any(feature = "ocl", test))]
const AUTOTUNE_MIN_VALID_SAMPLES: u32 = 5;
#[cfg(any(feature = "ocl", test))]
const AUTOTUNE_ECO_MIN_PERFORMANCE_RATIO: f64 = 0.70;
#[cfg(any(feature = "ocl", test))]
const AUTOTUNE_VERIFY_MIN_HPS_RATIO: f64 = 0.70;

#[cfg(any(feature = "ocl", test))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct BenchmarkMeasurement {
    hps: f64,
    samples: u32,
}

#[cfg(any(feature = "ocl", test))]
fn finish_benchmark_measurement(
    total_hashes: u64,
    total_secs: f64,
    samples: u32,
) -> Result<BenchmarkMeasurement, String> {
    if total_hashes == 0 {
        return Err("zero hashes measured".to_string());
    }
    if samples < AUTOTUNE_MIN_VALID_SAMPLES {
        return Err(format!(
            "only {samples} valid samples (minimum {AUTOTUNE_MIN_VALID_SAMPLES})"
        ));
    }
    if !total_secs.is_finite() || total_secs <= 0.0 {
        return Err("invalid measured duration".to_string());
    }
    let hps = total_hashes as f64 / total_secs;
    if !hps.is_finite() || hps <= 0.0 {
        return Err("zero or invalid hashrate".to_string());
    }
    Ok(BenchmarkMeasurement { hps, samples })
}

#[cfg(any(feature = "ocl", test))]
fn x16rs_algorithm_id(hash: &[u8; 32]) -> u8 {
    (u32::from_le_bytes([hash[28], hash[29], hash[30], hash[31]]) % 16) as u8
}

#[cfg(any(feature = "ocl", test))]
fn validate_benchmark_batch_result(
    height: u64,
    block_intro: &[u8],
    nonce_start: u32,
    batch: u32,
    result_nonce: u32,
    result_hash: &[u8; 32],
) -> Result<(), String> {
    if batch == 0 {
        return Err("zero-size OpenCL batch".to_string());
    }
    if result_hash.iter().all(|byte| *byte == 0) {
        return Err("OpenCL returned a zero hash result".to_string());
    }
    if *result_hash == [u8::MAX; 32] {
        return Err("OpenCL returned no best hash".to_string());
    }
    if result_nonce.wrapping_sub(nonce_start) >= batch {
        return Err(format!(
            "OpenCL returned nonce {result_nonce} outside batch starting at {nonce_start}"
        ));
    }
    if block_intro.len() < 83 {
        return Err("benchmark block intro is too short".to_string());
    }
    let mut verify_intro = block_intro.to_vec();
    verify_intro[79..83].copy_from_slice(&result_nonce.to_be_bytes());
    let expected_hash = x16rs::block_hash(height, &verify_intro);
    if expected_hash != *result_hash {
        let prehash = x16rs::calculate_hash(&verify_intro);
        return Err(format!(
            "OpenCL nonce/hash result failed CPU verification: nonce={result_nonce} algorithm={} gpu={} cpu={}",
            x16rs_algorithm_id(&prehash),
            hex::encode(result_hash),
            hex::encode(expected_hash)
        ));
    }
    Ok(())
}

#[cfg(feature = "ocl")]
fn execute_benchmark_batch(
    opencl: &crate::opencl_gpu::OpenCLResources,
    cnf: &PoWorkConf,
    height: u64,
    block_intro: &[u8],
    nonce_start: u32,
    batch: u32,
    wg_eff: u32,
    us: u32,
) -> Result<f64, String> {
    let started = Instant::now();
    let (result_nonce, result_hash) = do_group_block_mining_opencl(
        opencl,
        height,
        block_intro.to_vec(),
        nonce_start,
        wg_eff,
        cnf.localsize,
        us,
    )
    .map_err(|error| error.display())?;
    let used = started.elapsed().as_secs_f64();
    if !used.is_finite() || used <= 0.0 {
        return Err("OpenCL batch returned an invalid duration".to_string());
    }
    validate_benchmark_batch_result(
        height,
        block_intro,
        nonce_start,
        batch,
        result_nonce,
        &result_hash,
    )?;
    Ok(used)
}

#[cfg(feature = "ocl")]
fn bench_block_hps(
    opencl: &crate::opencl_gpu::OpenCLResources,
    cnf: &PoWorkConf,
    wg_eff: u32,
    us: u32,
    seconds: u64,
) -> Result<BenchmarkMeasurement, String> {
    let height = 1u64;
    let block_intro = BlockIntro::default().serialize();
    let batch_u64 = (wg_eff as u64)
        .saturating_mul(cnf.localsize as u64)
        .saturating_mul(us as u64);
    if batch_u64 == 0 || batch_u64 > u32::MAX as u64 {
        return Err(format!(
            "invalid launch size wg={wg_eff} local={} unit_size={us}",
            cnf.localsize
        ));
    }
    let batch = batch_u64 as u32;
    let mut nonce = 0u32;
    for warmup in 0..AUTOTUNE_WARMUP_BATCHES {
        execute_benchmark_batch(opencl, cnf, height, &block_intro, nonce, batch, wg_eff, us)
            .map_err(|error| format!("warm-up batch {} failed: {error}", warmup + 1))?;
        nonce = nonce.wrapping_add(batch);
    }

    let deadline = Instant::now() + Duration::from_secs(seconds.max(3));
    let mut total_hashes = 0u64;
    let mut total_secs = 0.0f64;
    let mut success_batches = 0u32;
    while Instant::now() < deadline {
        let used =
            execute_benchmark_batch(opencl, cnf, height, &block_intro, nonce, batch, wg_eff, us)
                .map_err(|error| {
                    format!("measured batch {} failed: {error}", success_batches + 1)
                })?;
        success_batches += 1;
        total_hashes = total_hashes.saturating_add(batch as u64);
        total_secs += used;
        nonce = nonce.wrapping_add(batch);
    }
    finish_benchmark_measurement(total_hashes, total_secs, success_batches)
}

#[cfg(any(feature = "ocl", test))]
#[derive(Clone, Debug)]
struct ProfileBenchResult {
    pick: BenchmarkPick,
    hps: f64,
    estimated_watts: f64,
    estimated_kh_per_j: f64,
    samples: u32,
}

#[cfg(any(feature = "ocl", test))]
impl ProfileBenchResult {
    fn new(pick: BenchmarkPick, hps: f64, estimated_watts: f64, samples: u32) -> Option<Self> {
        if !hps.is_finite()
            || hps <= 0.0
            || !estimated_watts.is_finite()
            || estimated_watts <= 0.0
            || samples < AUTOTUNE_MIN_VALID_SAMPLES
        {
            return None;
        }
        Some(Self {
            pick,
            hps,
            estimated_watts,
            estimated_kh_per_j: hps / estimated_watts / 1000.0,
            samples,
        })
    }
}

#[cfg(any(feature = "ocl", test))]
fn pick_benchmark_result(
    results: &[ProfileBenchResult],
    mode: EfficiencyMode,
) -> Option<&ProfileBenchResult> {
    let stable = || {
        results.iter().filter(|result| {
            result.hps.is_finite()
                && result.hps > 0.0
                && result.estimated_watts.is_finite()
                && result.estimated_watts > 0.0
                && result.estimated_kh_per_j.is_finite()
                && result.estimated_kh_per_j > 0.0
                && result.samples >= AUTOTUNE_MIN_VALID_SAMPLES
        })
    };
    match mode {
        EfficiencyMode::Max => stable().max_by(|a, b| a.hps.total_cmp(&b.hps)),
        EfficiencyMode::Profit => {
            stable().max_by(|a, b| a.estimated_kh_per_j.total_cmp(&b.estimated_kh_per_j))
        }
        EfficiencyMode::Eco => {
            let max_hps = stable().map(|result| result.hps).fold(0.0f64, f64::max);
            let minimum_hps = max_hps * AUTOTUNE_ECO_MIN_PERFORMANCE_RATIO;
            stable()
                .filter(|result| result.hps >= minimum_hps)
                .min_by(|a, b| {
                    a.estimated_watts
                        .total_cmp(&b.estimated_watts)
                        .then_with(|| b.hps.total_cmp(&a.hps))
                })
        }
    }
}

#[cfg(any(feature = "ocl", test))]
fn verification_is_stable(expected_hps: f64, verified_hps: f64) -> bool {
    expected_hps.is_finite()
        && expected_hps > 0.0
        && verified_hps.is_finite()
        && verified_hps >= expected_hps * AUTOTUNE_VERIFY_MIN_HPS_RATIO
}

#[cfg(any(feature = "ocl", test))]
fn verification_seconds(total_secs: u64) -> u64 {
    (total_secs / 4).clamp(5, 15)
}

#[cfg(any(feature = "ocl", test))]
fn autotune_device_count_is_supported(device_count: usize) -> bool {
    device_count == 1
}
#[cfg(feature = "ocl")]
fn benchmark_candidate(
    opencl: &crate::opencl_gpu::OpenCLResources,
    cnf: &PoWorkConf,
    pick: BenchmarkPick,
    seconds: u64,
    max_workgroups: u32,
    max_unitsize: u32,
) -> Result<ProfileBenchResult, String> {
    let measurement = bench_block_hps(opencl, cnf, pick.workgroups, pick.unitsize, seconds)?;
    let estimated_watts = cnf.efficiency.estimate_tuning_watts(
        &pick.profile,
        pick.workgroups,
        pick.unitsize,
        max_workgroups,
        max_unitsize,
    );
    ProfileBenchResult::new(pick, measurement.hps, estimated_watts, measurement.samples)
        .ok_or_else(|| "candidate produced an invalid measurement".to_string())
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
        println!(
            "[benchmark] Power and kH/J figures are estimates derived from configured board power; they are not hardware telemetry."
        );
        println!(
            "[benchmark] NOTE: the MH/s below are raw X16RS repeat=1 tuning rates (relative comparison only). The live mainnet runs 16 rounds, so real block-hash throughput is roughly 1/11-1/16 of these numbers. For the honest mainnet figure run: set HACASH_REPEAT16_BENCH_SECONDS=30 and run poworker."
        );
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

        // Allocate GPU buffers for the largest profile unit_size (amd_max uses 128).
        let init_unitsize = cnf.unitsize.max(128);
        let scan = crate::opencl_diag::scan_opencl();
        let opencl_resources = initialize_opencl(
            false,
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
            println!("[benchmark] No OpenCL devices");
            return;
        }
        if !autotune_device_count_is_supported(opencl_resources.len()) {
            println!(
                "[benchmark] Auto Tune requires exactly one OpenCL device, but detected {}. The current config has one shared work_groups/unit_size pair, so multi-GPU tuning would be ambiguous. Set [gpu] device_ids to one device and tune each GPU separately; config unchanged.",
                opencl_resources.len()
            );
            return;
        }

        for (dev_i, opencl) in opencl_resources.iter().enumerate() {
            let limits = crate::gpu_arch::ArchLimits::for_slug(&opencl.arch_slug);
            let min_wg = limits.panel_min_wg.min(opencl.workgroups);
            let max_wg = opencl.workgroups;
            let max_us = limits
                .max_unit_size()
                .min(opencl.allocated_unitsize)
                .max(32);
            let max_tier = crate::gpu_arch::ArchLimits::panel_max_tier(&cnf.gpu_slug);
            let candidates = benchmark_candidates_for_device(
                opencl.vendor,
                min_profile_tier_for_mode(cnf.efficiency.mode),
                max_tier,
                min_wg,
                max_wg,
                max_us,
            );
            if candidates.is_empty() {
                println!(
                    "[benchmark] No safe tuning candidates for device #{}",
                    dev_i
                );
                continue;
            }
            let per = (profile_secs / candidates.len() as u64).max(4);
            println!(
                "[benchmark] Device #{}: {}s x {} exact tuning points{}",
                dev_i,
                per,
                candidates.len(),
                if fine { " + bounded fine sweep" } else { "" }
            );

            let mut bench_results = Vec::new();
            for pick in candidates {
                match benchmark_candidate(opencl, cnf, pick.clone(), per, max_wg, max_us) {
                    Ok(result) => {
                        println!(
                            "[benchmark] dev{} {}: {} (estimated {:.1} kH/J @ {:.0}W, {} samples, wg={}, unit_size={})",
                            dev_i,
                            result.pick.profile,
                            rates_to_show(result.hps),
                            result.estimated_kh_per_j,
                            result.estimated_watts,
                            result.samples,
                            result.pick.workgroups,
                            result.pick.unitsize
                        );
                        bench_results.push(result);
                    }
                    Err(error) => {
                        println!(
                            "[benchmark] dev{} {}: REJECTED ({error}, wg={}, unit_size={})",
                            dev_i, pick.profile, pick.workgroups, pick.unitsize
                        );
                    }
                }
            }

            let Some(base) = pick_benchmark_result(&bench_results, cnf.efficiency.mode) else {
                println!(
                    "[benchmark] No successful tuning points — config unchanged (check OpenCL driver)."
                );
                continue;
            };
            let mut selected = base.clone();

            if fine && sweep_secs > 0 {
                let wg_sweep_secs = sweep_secs / 2;
                let us_sweep_secs = sweep_secs.saturating_sub(wg_sweep_secs).max(6);
                let wg_candidates = sweep_workgroup_candidates_bounded(
                    selected.pick.workgroups,
                    opencl.vram_bytes,
                    cnf.localsize,
                    selected.pick.unitsize,
                    min_wg,
                    max_wg,
                );
                let per_wg = (wg_sweep_secs / wg_candidates.len().max(1) as u64).max(3);
                println!(
                    "[benchmark] dev{} bounded wg sweep: {:?} x {}s",
                    dev_i, wg_candidates, per_wg
                );
                let mut wg_results = Vec::new();
                for wg_try in wg_candidates {
                    let candidate = BenchmarkPick {
                        profile: selected.pick.profile.clone(),
                        workgroups: wg_try,
                        unitsize: selected.pick.unitsize,
                    };
                    match benchmark_candidate(opencl, cnf, candidate, per_wg, max_wg, max_us) {
                        Ok(result) => {
                            println!(
                                "[benchmark] dev{} wg={}: {} (estimated {:.1} kH/J @ {:.0}W, {} samples)",
                                dev_i,
                                wg_try,
                                rates_to_show(result.hps),
                                result.estimated_kh_per_j,
                                result.estimated_watts,
                                result.samples
                            );
                            wg_results.push(result);
                        }
                        Err(error) => {
                            println!("[benchmark] dev{} wg={}: REJECTED ({error})", dev_i, wg_try);
                        }
                    }
                }
                if let Some(best) = pick_benchmark_result(&wg_results, cnf.efficiency.mode) {
                    selected = best.clone();
                }

                let us_candidates = sweep_unitsize_candidates(selected.pick.unitsize, max_us);
                let per_us = (us_sweep_secs / us_candidates.len().max(1) as u64).max(3);
                println!(
                    "[benchmark] dev{} bounded unit_size sweep: {:?} x {}s",
                    dev_i, us_candidates, per_us
                );
                let mut us_results = Vec::new();
                for us_try in us_candidates {
                    let candidate = BenchmarkPick {
                        profile: selected.pick.profile.clone(),
                        workgroups: selected.pick.workgroups,
                        unitsize: us_try,
                    };
                    match benchmark_candidate(opencl, cnf, candidate, per_us, max_wg, max_us) {
                        Ok(result) => {
                            println!(
                                "[benchmark] dev{} unit_size={}: {} (estimated {:.1} kH/J @ {:.0}W, {} samples)",
                                dev_i,
                                us_try,
                                rates_to_show(result.hps),
                                result.estimated_kh_per_j,
                                result.estimated_watts,
                                result.samples
                            );
                            us_results.push(result);
                        }
                        Err(error) => {
                            println!(
                                "[benchmark] dev{} unit_size={}: REJECTED ({error})",
                                dev_i, us_try
                            );
                        }
                    }
                }
                if let Some(best) = pick_benchmark_result(&us_results, cnf.efficiency.mode) {
                    selected = best.clone();
                }
            }

            let verify_secs = verification_seconds(total_secs);
            println!(
                "[benchmark] dev{} final verification soak: {}s at wg={} unit_size={}",
                dev_i, verify_secs, selected.pick.workgroups, selected.pick.unitsize
            );
            let verified = match benchmark_candidate(
                opencl,
                cnf,
                selected.pick.clone(),
                verify_secs,
                max_wg,
                max_us,
            ) {
                Ok(result) => result,
                Err(error) => {
                    println!(
                        "[benchmark] dev{} final verification REJECTED ({error}) - config unchanged.",
                        dev_i
                    );
                    continue;
                }
            };
            if !verification_is_stable(selected.hps, verified.hps) {
                println!(
                    "[benchmark] dev{} final verification REJECTED: {} is below {:.0}% of measured {} - config unchanged.",
                    dev_i,
                    rates_to_show(verified.hps),
                    AUTOTUNE_VERIFY_MIN_HPS_RATIO * 100.0,
                    rates_to_show(selected.hps)
                );
                continue;
            }
            println!(
                "[benchmark] dev{} verified: profile={} work_groups={} unit_size={} {} (estimated {:.1} kH/J @ {:.0}W, {} samples, mode={})",
                dev_i,
                verified.pick.profile,
                verified.pick.workgroups,
                verified.pick.unitsize,
                rates_to_show(verified.hps),
                verified.estimated_kh_per_j,
                verified.estimated_watts,
                verified.samples,
                cnf.efficiency.mode.label()
            );
            if dev_i == 0 {
                match apply_benchmark_pick(config_path, &verified.pick) {
                    Ok(()) => {
                        println!(
                            "[benchmark] Config updated only after successful final verification."
                        )
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

    fn bench_result(
        profile: &str,
        workgroups: u32,
        unitsize: u32,
        hps: f64,
        estimated_watts: f64,
    ) -> ProfileBenchResult {
        ProfileBenchResult::new(
            BenchmarkPick {
                profile: profile.to_string(),
                workgroups,
                unitsize,
            },
            hps,
            estimated_watts,
            AUTOTUNE_MIN_VALID_SAMPLES,
        )
        .unwrap()
    }

    #[test]
    fn autotune_pick_preserves_the_exact_measured_values() {
        let results = vec![
            bench_result("amd_profit", 1024, 96, 100.0, 100.0),
            bench_result("amd_performance", 1536, 64, 120.0, 150.0),
        ];
        assert_eq!(
            pick_benchmark_result(&results, EfficiencyMode::Max)
                .unwrap()
                .pick,
            results[1].pick
        );
        assert_eq!(
            pick_benchmark_result(&results, EfficiencyMode::Profit)
                .unwrap()
                .pick,
            results[0].pick
        );
    }

    #[test]
    fn autotune_selection_is_mode_aware() {
        let results = vec![
            bench_result("amd_eco", 32, 32, 60.0, 20.0),
            bench_result("amd_balanced", 48, 48, 75.0, 40.0),
            bench_result("amd_performance", 64, 64, 100.0, 80.0),
        ];

        assert_eq!(
            pick_benchmark_result(&results, EfficiencyMode::Max)
                .unwrap()
                .pick
                .profile,
            "amd_performance"
        );
        assert_eq!(
            pick_benchmark_result(&results, EfficiencyMode::Profit)
                .unwrap()
                .pick
                .profile,
            "amd_eco"
        );
        assert_eq!(
            pick_benchmark_result(&results, EfficiencyMode::Eco)
                .unwrap()
                .pick
                .profile,
            "amd_balanced"
        );
    }

    #[test]
    fn autotune_rejects_zero_and_under_sampled_measurements() {
        assert!(finish_benchmark_measurement(0, 1.0, 10).is_err());
        assert!(
            finish_benchmark_measurement(1_000, 1.0, AUTOTUNE_MIN_VALID_SAMPLES.saturating_sub(1))
                .is_err()
        );
        let valid = finish_benchmark_measurement(1_000, 2.0, AUTOTUNE_MIN_VALID_SAMPLES).unwrap();
        assert_eq!(valid.hps, 500.0);
        assert_eq!(valid.samples, AUTOTUNE_MIN_VALID_SAMPLES);
    }

    #[test]
    fn autotune_final_verification_requires_repeatable_hashrate() {
        assert!(verification_is_stable(100.0, 70.0));
        assert!(!verification_is_stable(100.0, 69.99));
        assert!(!verification_is_stable(100.0, f64::NAN));
        assert_eq!(verification_seconds(15), 5);
        assert_eq!(verification_seconds(60), 15);
        assert_eq!(verification_seconds(600), 15);
    }

    #[test]
    fn autotune_rejects_ambiguous_multi_gpu_targets() {
        assert!(!autotune_device_count_is_supported(0));
        assert!(autotune_device_count_is_supported(1));
        assert!(!autotune_device_count_is_supported(2));
    }

    #[test]
    fn autotune_rejects_invalid_gpu_results_and_accepts_cpu_verified_result() {
        let height = 1u64;
        let block_intro = BlockIntro::default().serialize();
        let nonce_start = 11u32;
        let batch = 256u32;

        assert!(
            validate_benchmark_batch_result(
                height,
                &block_intro,
                nonce_start,
                batch,
                nonce_start,
                &[0u8; 32]
            )
            .is_err()
        );
        assert!(
            validate_benchmark_batch_result(
                height,
                &block_intro,
                nonce_start,
                batch,
                nonce_start,
                &[u8::MAX; 32]
            )
            .is_err()
        );

        let result_nonce = nonce_start + 42;
        let mut verified_intro = block_intro.clone();
        verified_intro[79..83].copy_from_slice(&result_nonce.to_be_bytes());
        let result_hash = x16rs::block_hash(height, &verified_intro);
        validate_benchmark_batch_result(
            height,
            &block_intro,
            nonce_start,
            batch,
            result_nonce,
            &result_hash,
        )
        .unwrap();

        assert!(
            validate_benchmark_batch_result(
                height,
                &block_intro,
                nonce_start,
                batch,
                nonce_start + batch,
                &result_hash
            )
            .is_err()
        );
    }

    #[test]
    fn gfx1201_groestl_failure_vector_is_cpu_rejected() {
        let height = 1u64;
        let block_intro = BlockIntro::default().serialize();
        let result_nonce = 6_858_338u32;
        let mut verified_intro = block_intro.clone();
        verified_intro[79..83].copy_from_slice(&result_nonce.to_be_bytes());
        let pre_x16rs = x16rs::calculate_hash(&verified_intro);
        let expected = x16rs::block_hash(height, &verified_intro);
        let bad_gpu_hash: [u8; 32] =
            hex::decode("00004f8f9d0fd569407298186d7015bc19d70bd379a551190b7233135562cb33")
                .unwrap()
                .try_into()
                .unwrap();

        println!(
            "nonce={result_nonce} pre_x16rs={} algorithm={} expected_x16rs={} bad_gpu={}",
            hex::encode(pre_x16rs),
            x16rs_algorithm_id(&pre_x16rs),
            hex::encode(expected),
            hex::encode(bad_gpu_hash)
        );
        assert_eq!(x16rs_algorithm_id(&pre_x16rs), 2);
        assert!(
            validate_benchmark_batch_result(
                height,
                &block_intro,
                result_nonce,
                1,
                result_nonce,
                &bad_gpu_hash
            )
            .is_err()
        );
    }

    #[test]
    fn cpu_group_mining_result_matches_manual_scan() {
        let height = 1u64;
        let block_intro = BlockIntro::default().serialize();
        let nonce_start = 11u32;
        let nonce_space = 256u32;

        let (best_nonce, best_hash) = block_mining_runtime::do_group_block_mining(
            height,
            block_intro.clone(),
            nonce_start,
            nonce_space,
        );

        let mut manual_nonce = 0u32;
        let mut manual_hash = [255u8; 32];
        let mut intro = block_intro;
        for nonce in nonce_start..nonce_start + nonce_space {
            intro[79..83].copy_from_slice(&nonce.to_be_bytes());
            let hx = x16rs::block_hash(height, &intro);
            if crate::hash_util::hash_more_power(&hx, &manual_hash) {
                manual_hash = hx;
                manual_nonce = nonce;
            }
        }

        assert_eq!(best_nonce, manual_nonce);
        assert_eq!(best_hash, manual_hash);
    }
}
