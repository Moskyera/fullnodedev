use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::*};

use std::time::*;

use reqwest::blocking::Client as HttpClient;
use serde_json::Value as JV;

use crate::efficiency::*;

use sys::*;

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
        wlogln!(
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
        wlogerr!("[Fatal] Mining worker startup failed.");
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

/// Per-request long-poll wait for the miner notice. When a supervisor can ask us
/// to stop, cap it so the loop re-checks the stop flag every few seconds instead
/// of blocking for the whole configured notice window.
const NOTICE_POLL_WAIT_WITH_STOP: u64 = 3;

/// How long to wait before polling again while the upstream says it is serving
/// work it can no longer refresh. Long enough not to hammer a struggling pool or
/// node, short enough to pick mining back up as soon as the outage clears.
const STALE_UPSTREAM_BACKOFF_SECS: u64 = 10;

/// Minimum period of one pending + notice cycle. A cooperating upstream already
/// blocks inside the notice long-poll for the configured wait, so this floor never
/// slows a healthy miner down. It exists only for the pathological upstream: one
/// that answers the notice immediately (a saturated pool bridge, or a server with
/// no long-poll at all). The cycle costs TWO requests, so without a floor a single
/// miner would issue tens of requests per second forever and make the overload it
/// is suffering from worse.
const NOTICE_CYCLE_MIN: Duration = Duration::from_millis(200);

/// Anti-spin sleep still owed by a notice cycle. Zero when the notice reported
/// that the tip reached the height we are mining on top of: genuinely new work is
/// money and is never delayed.
fn notice_cycle_backoff(spent: Duration, new_work_ready: bool) -> Duration {
    if new_work_ready {
        return Duration::ZERO;
    }
    NOTICE_CYCLE_MIN.saturating_sub(spent)
}

/// Detect the "upstream is serving work it can no longer refresh" answer that the
/// pool bridge returns on `/query/miner/pending` and `/query/miner/notice` during
/// an upstream node outage. Both bodies stay backward compatible in shape (a
/// plain `err` string), so anything else, including an unknown or malformed error
/// body, is treated as an ordinary transient error and must not crash the miner.
fn upstream_stale_reason(res: &JV) -> Option<&str> {
    let err = res["err"].as_str()?;
    if err.is_empty() {
        return None;
    }
    if err.to_ascii_lowercase().contains("stale") {
        Some(err)
    } else {
        None
    }
}

/// Report an upstream stale-work outage once per transition and pause the mining
/// threads: the installed template can no longer win a block or earn a share, so
/// continuing to hash it only burns power.
fn enter_upstream_stale(reason: &str, source: &str) {
    if block_mining_runtime::set_upstream_stale(true) {
        wlogln!(
            "\n[Mining] PAUSED: {source} reports stale work ({reason}). The template can no longer win anything, so hashing is idle until fresh work arrives. Check the pool or node this miner connects to."
        );
    }
}

/// Clear the stale-work pause once real work is available again.
fn leave_upstream_stale() {
    if block_mining_runtime::set_upstream_stale(false) {
        wlogln!("\n[Mining] Fresh work received, mining resumes.");
    }
}

fn notice_poll_wait(notice_wait: u64, can_stop: bool) -> u64 {
    if can_stop {
        notice_wait.min(NOTICE_POLL_WAIT_WITH_STOP)
    } else {
        notice_wait
    }
}

fn pull_pending_block_stuff(cnf: &PoWorkConf, stop_flag: &Option<Arc<AtomicBool>>) {
    let cycle_start = Instant::now();
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
                wlogln!(
                    "Error: cannot get block data at {}: {}\n",
                    &urlapi_pending, e
                );
                delay_return!(30);
            }
        };
    let Ok(res) = serde_json::from_str::<JV>(&jsdata) else {
        wlogln!(
            "Error: invalid block data json at {} (body len {})\n",
            &urlapi_pending,
            jsdata.len()
        );
        delay_return!(10);
    };
    let jstr = |k| res[k].as_str().unwrap_or("");
    let jnum = |k| res[k].as_u64().unwrap_or(0);
    let JV::String(ref _blkhd) = res["block_intro"] else {
        if let Some(reason) = upstream_stale_reason(&res) {
            enter_upstream_stale(reason, "pending work");
            delay_return!(STALE_UPSTREAM_BACKOFF_SECS);
        }
        wlogln!("Error: get block stuff error: {}", jstr("err"));
        delay_return!(15);
    };
    let pending_height = jnum("height");

    // Install the refreshed template unconditionally. Comparing the raw block_intro
    // hex to decide would call EVERY poll a template change: the fullnode
    // increments the served coinbase nonce and recomputes the merkle root on every
    // /query/miner/pending request, and the merkle root lives inside the serialized
    // intro. Whether this is really a NEW job - and so whether workers must give up
    // what they are mining - is decided inside set_pending_block_stuff from the
    // job's identity (height + parent hash), which that re-serialization leaves
    // untouched. A same-height reorg still changes the parent and is still detected.
    if let Err(e) = block_mining_runtime::set_pending_block_stuff(pending_height, res) {
        wlogln!("Error: invalid block data from {urlapi_pending}: {e}");
        delay_return!(10);
    }
    // Real work was served and installed, so any stale-work pause lifts.
    leave_upstream_stale();
    if curr_hei == 0 {
        block_mining_runtime::may_print_turn_to_nex_block_mining(curr_hei, None);
    }

    // with notice
    let mut rpid = vec![0].repeat(16);
    // The stop flag is only observable BETWEEN notice requests, so cap the
    // per-request wait when a supervisor can ask us to stop (see
    // notice_poll_wait); otherwise an in-flight long-poll would hold shutdown
    // for the whole notice window.
    let poll_wait = notice_poll_wait(cnf.noticewait, stop_flag.is_some());
    let poll_timeout = if stop_flag.is_some() {
        Duration::from_secs(poll_wait.saturating_add(5))
    } else {
        Duration::from_secs(300)
    };
    loop {
        if should_stop(stop_flag) {
            return;
        }
        if let Err(e) = getrandom::fill(&mut rpid) {
            wlogln!("Error: cannot generate request id: {e}");
            delay_return!(1);
        }
        let urlapi_notice = format!(
            "http://{}/query/miner/notice?wait={}&height={}&rqid={}",
            &cnf.rpcaddr,
            &poll_wait,
            pending_height,
            &hex::encode(&rpid)
        );
        // wlogln!("\n-------- {} -------- {}\n", &ctshow(), &urlapi_notice);
        let jsdata = match crate::rpc_http::get_text(
            &HTTP_CLIENT,
            &urlapi_notice,
            &cnf.api_token,
            Some(poll_timeout),
        ) {
            Ok(t) => t,
            Err(e) => {
                wlogln!(
                    "Error: cannot get miner notice at {}: {}\n",
                    &urlapi_notice, e
                );
                delay_return!(10);
            }
        };
        let Ok(res2) = serde_json::from_str::<JV>(&jsdata) else {
            wlogln!("Error: invalid miner notice JSON at {urlapi_notice}");
            delay_return!(1);
        };
        // The notice body carries the LAST known height alongside the error, so it
        // must be checked before the height comparison below: otherwise a stale
        // notice at our own height would look like new work and spin the miner
        // between notice and pending while the workers grind dead work.
        if let Some(reason) = upstream_stale_reason(&res2) {
            enter_upstream_stale(reason, "block notice");
            delay_return!(STALE_UPSTREAM_BACKOFF_SECS);
        }
        let jnum = |k| res2[k].as_u64().unwrap_or(0);
        let res_hei = jnum("height");
        // Fullnode notice reports chain tip height, while we long-poll with the
        // pending (tip+1) height. When tip advances to pending_height, break for
        // the next job. On timeout (or any other reply) also leave the wait so the
        // outer loop re-fetches /query/miner/pending: that is how same-height tip
        // reorgs (intro change without tip height advance) are detected. A second
        // tight spin on the same tip would never re-read block_intro.
        //
        // Nothing new means the cycle must not be allowed to run free: it costs two
        // requests, and an upstream that answers the notice instantly would
        // otherwise be hammered at tens of requests per second (see NOTICE_CYCLE_MIN).
        let new_work_ready = pending_height > 0 && res_hei >= pending_height;
        let backoff = notice_cycle_backoff(cycle_start.elapsed(), new_work_ready);
        if !backoff.is_zero() {
            std::thread::sleep(backoff);
        }
        break;
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
    // result was already drained from the channel, so a single transient network
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
                        wlogln!("[submit] node rejected height {}: {}", success.height, err);
                        break;
                    }
                    None => {
                        // HTTP 200 but no parseable `ret` (proxy/load-balancer error
                        // page, truncated or non-JSON body). This is NOT a node
                        // decision, so treat it as transient and retry: a winning
                        // block is not discarded on a front-end hiccup.
                        let snippet: String = body.chars().take(120).collect();
                        wlogln!(
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
                wlogln!(
                    "[submit] attempt {}/{} failed: {e}",
                    attempt, MAX_SUBMIT_ATTEMPTS
                );
                if attempt < MAX_SUBMIT_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(500u64 * attempt as u64));
                }
            }
        }
    }
    wlogln!("{} {}", &urlapi_success, last);
    if accepted {
        wlogln!(
            "\n\n████████████████ [MINING SUCCESS] Find a block height {},\n██ hash {} to submit.",
            success.height,
            success.result_hash.to_hex()
        );
    } else {
        wlogln!(
            "\n\n████████████████ [MINING SUBMIT FAILED] block height {} was NOT confirmed accepted\n██ after {} attempts (hash {}). Check the node/connection.",
            success.height,
            MAX_SUBMIT_ATTEMPTS,
            success.result_hash.to_hex()
        );
    }
    wlogln!("▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔")
}
/// How many rank thresholds each candidate's equivalence proof uses.
///
/// Each threshold is one launch that reads every hash in the window, and the
/// set of them catches ONE wrong hash anywhere in the window with probability
/// 1 - p, where p is printed by the tuner. 31 keeps a candidate's proof under a
/// second on this card while leaving a shape bug, which corrupts many hashes and
/// not one, with no way through: the miss probabilities multiply.
#[cfg(any(feature = "ocl", feature = "cuda"))]
const AUTOTUNE_PROOF_THRESHOLDS: u32 = 31;

/// Corpus headers. Four distinct block intros, so the tune cannot be an artefact
/// of one header's algorithm chain.
#[cfg(any(feature = "ocl", feature = "cuda"))]
const AUTOTUNE_CORPUS_HEADERS: u32 = 4;

/// What a hash is worth per day, read from the node the miner is configured to
/// mine on, or `None` when that cannot be known honestly.
///
/// Only Profit mode needs it, and only to compare candidates. It is deliberately
/// not attempted when a pool is configured: a pool serves a SHARE target, which
/// is orders of magnitude weaker than the network target, so pricing a hash from
/// it would overstate revenue enormously and make every extra watt look free.
#[cfg(any(feature = "ocl", feature = "cuda"))]
fn network_hac_per_hps_day(cnf: &PoWorkConf) -> Option<f64> {
    if !cnf.pool_worker.is_empty() {
        wlogln!(
            "[autotune] pool_worker is set, so the target this miner is served is a share target, \
             not the network's. The value of a hash cannot be read from it."
        );
        return None;
    }
    let url = format!(
        "http://{}/query/miner/pending?stuff=true&t={}",
        &cnf.rpcaddr,
        sys::curtimes()
    );
    let body = crate::rpc_http::get_text(&HTTP_CLIENT, &url, &cnf.api_token, None).ok()?;
    let res: JV = serde_json::from_str(&body).ok()?;
    let height = res["height"].as_u64()?;
    let target = hex::decode(res["target_hash"].as_str()?).ok()?;
    let value = block_mining_runtime::hac_per_day_per_hashrate(height, &target)?;
    wlogln!(
        "[autotune] value of a hash, from {} at height {}: {:.3e} HAC per H/s per day",
        &cnf.rpcaddr,
        height,
        value
    );
    Some(value)
}

/// Which backend a tune has to measure, decided by the same rule the MINER is
/// built from.
///
/// `build_gpu_backends` prefers CUDA (`if cnf.usecuda { .. } else if
/// cnf.useopencl { .. }`) and hands `cnf.workgroups` / `cnf.unitsize` to
/// `initialize_cuda`, while `apply_benchmark_pick` writes exactly those two
/// numbers. So the tuner must measure whatever the next run will mine on, and
/// never the other backend: a tune that ran on OpenCL with `use_cuda = true`
/// still in the file would land its winner in the numbers the CUDA miner is
/// built from, and the two grids are three orders of magnitude apart (the
/// shipped CUDA config starts at 131072 x 256 x 8; the OpenCL grid tops out in
/// the low thousands of work groups).
///
/// This used to be a refusal. It is now a route, because both backends can be
/// measured.
fn tuning_backend(cnf: &PoWorkConf) -> Option<&'static str> {
    if cnf.usecuda {
        Some("cuda")
    } else if cnf.useopencl {
        Some("opencl")
    } else {
        None
    }
}

/// Everything the tuner needs about money, which is the same on both backends.
#[cfg(any(feature = "ocl", feature = "cuda"))]
fn autotune_economics(cnf: &PoWorkConf) -> crate::autotune16::Economics {
    crate::autotune16::Economics {
        power_cost_kwh: cnf.efficiency.power_cost_kwh,
        hac_price: cnf.efficiency.hac_price,
        hac_per_hps_day: if cnf.efficiency.mode == EfficiencyMode::Profit {
            network_hac_per_hps_day(cnf)
        } else {
            None
        },
        cpu_watts: cnf.efficiency.initial_active_supervene(cnf.supervene) as f64
            * cnf.efficiency.cpu_watts_per_thread,
    }
}

/// The CPU oracle is what proves each shape, and it is the only part of a tune
/// that competes with the rest of the machine. Two cores are left alone so that
/// a node syncing beside the tuner keeps running.
#[cfg(any(feature = "ocl", feature = "cuda"))]
fn autotune_oracle_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
}

/// Report the tune and patch the ini, or say why it did not.
///
/// Shared by both backends deliberately: "the card never settled, so the config
/// is unchanged" is a statement about the soak, not about the GPU API, and a
/// CUDA operator has to get exactly the same guarantee an OpenCL one does.
#[cfg(any(feature = "ocl", feature = "cuda"))]
fn finish_tune(
    config_path: &str,
    request: &crate::autotune16::TuneRequest,
    outcome: &crate::autotune16::TuneOutcome,
) {
    wlogln!("\n================ AUTO TUNE (x16rs repeat = 16) ================");
    wlogln!("{}", crate::autotune16::render(outcome, request));
    if !outcome.settle.settled {
        // The soak has already said which of the two it was, and they have
        // different answers. A corpus that could not fit the settling window is
        // a planning failure, and `plan_session` refuses those before the sweep,
        // so reaching here means the card really was still moving.
        wlogln!(
            "[autotune] The card never settled, so this shape is not proven to sustain. Config \
             unchanged. The soak made {} passes over {:.0}s with the hashrate still spanning \
             {:.2}%; a larger benchmark_seconds buys a longer soak (up to 900s) and is the thing \
             to try.",
            outcome.soak.len(),
            outcome.soak_seconds,
            outcome.settle.rate_span_pct,
        );
        return;
    }
    match apply_benchmark_pick(config_path, &outcome.pick) {
        Ok(()) => {
            wlogln!("[autotune] Config updated only after a settled soak at the chosen shape.")
        }
        Err(error) => wlogln!("[autotune] Could not patch ini: {error}"),
    }
}

fn run_block_mining_benchmark(cnf: &PoWorkConf, config_path: &str) {
    match tuning_backend(cnf) {
        Some("cuda") => run_cuda_benchmark(cnf, config_path),
        Some("opencl") => run_opencl_benchmark(cnf, config_path),
        _ => wlogln!(
            "[autotune] Neither [gpu] use_cuda nor [gpu] use_opencl is set, so there is no GPU \
             backend to measure. Config unchanged."
        ),
    }
}

/// Auto Tune on a build with no CUDA backend at all.
///
/// The message names CUDA, because that is what the operator asked for. The
/// version this replaced told a CUDA owner to enable OpenCL, a backend they had
/// deliberately turned off, and never mentioned CUDA at all.
#[cfg(not(feature = "cuda"))]
fn run_cuda_benchmark(cnf: &PoWorkConf, config_path: &str) {
    let _ = (cnf, config_path);
    wlogln!(
        "[autotune] This config has [gpu] use_cuda = true, so the miner runs on CUDA, but THIS \
         BINARY was built without the CUDA backend and has no way to open an NVIDIA device or to \
         measure one. Nothing was measured and the config is unchanged. Rebuild with \
         `cargo build --release --features cuda` (the CUDA Toolkit must be installed and CUDA_PATH \
         set, or the build produces a binary with the feature and no kernels), or set use_cuda = \
         false and use_opencl = true to tune the OpenCL backend instead."
    );
}

/// Auto Tune on CUDA. Same tuner, same corpus, same proof, same soak.
#[cfg(feature = "cuda")]
fn run_cuda_benchmark(cnf: &PoWorkConf, config_path: &str) {
    // The kernels, not the feature. `--features cuda` only adds the crate;
    // whether it holds compiled kernels was decided by its build script finding
    // nvcc, and without them every device call returns NotCompiled. Said in
    // words about nvcc rather than as a driver error.
    if !crate::x16rs_gate::cuda_kernels_available() {
        wlogln!(
            "[autotune] This binary has the cuda feature but NO CUDA KERNELS: x16rs-cuda's build \
             script did not find nvcc when it was built, so cfg(cuda_available) was never set and \
             every CUDA call returns `x16rs-cuda built without CUDA kernels`. Install the CUDA \
             Toolkit, set CUDA_PATH, and rebuild; the build prints `Using CUDA Toolkit at ...` \
             when it found one. Nothing was measured and the config is unchanged."
        );
        return;
    }

    let devices = match x16rs_cuda::CudaMiner::list_devices() {
        Ok(devices) => devices,
        Err(error) => {
            wlogln!(
                "[autotune] No CUDA device could be enumerated: {error}. This is the driver or the \
                 card, not the build: the kernels are compiled in. Check `nvidia-smi` runs, and \
                 that the driver is not older than the CUDA runtime this binary was built \
                 against. Nothing was measured and the config is unchanged."
            );
            return;
        }
    };
    if devices.is_empty() {
        wlogln!(
            "[autotune] The CUDA runtime reports zero devices on this machine. Nothing was \
             measured and the config is unchanged."
        );
        return;
    }
    if !devices.iter().any(|d| d.index == cnf.cudadevice) {
        wlogln!(
            "[autotune] [gpu] cuda_device = {} is not present. This machine has: {}. Set \
             cuda_device to one of those. Config unchanged.",
            cnf.cudadevice,
            devices
                .iter()
                .map(|d| format!("#{} {}", d.index, d.name))
                .collect::<Vec<_>>()
                .join(", ")
        );
        return;
    }

    let limits = match x16rs_cuda::CudaMiner::limits(cnf.cudadevice) {
        Ok(limits) => limits,
        Err(error) => {
            wlogln!(
                "[autotune] CUDA device #{} could not be probed: {error}. Config unchanged.",
                cnf.cudadevice
            );
            return;
        }
    };
    wlogln!("[autotune] CUDA {}", limits.describe());

    // The block size is not a tuning axis on this backend and cannot be made
    // one: `x16rs_cuda_main` declares `__shared__ unsigned int
    // local_nonces[256]` and reduces over a power-of-two tree across the block,
    // so 256 is the only block size it is correct at. Said out loud when the ini
    // disagrees, because the alternative is a tune whose answer silently does
    // not match the local_size the operator wrote.
    let local_size = x16rs_cuda::DEFAULT_LOCAL_SIZE;
    if cnf.localsize != local_size {
        wlogln!(
            "[autotune] [gpu] local_size = {} is ignored on CUDA: block_miner.cu's shared \
             local_nonces[{local_size}] and its tree reduction are correct at {local_size} threads \
             a block and no other, so the tune measures {local_size}.",
            cnf.localsize
        );
    }
    if limits.kernel_max_threads_per_block > 0
        && (limits.kernel_max_threads_per_block as u32) < local_size
    {
        wlogln!(
            "[autotune] the batch kernel supports only {} threads a block on this device but its \
             reduction requires {local_size}. This device cannot run the CUDA miner at all, so \
             there is nothing to tune. Config unchanged.",
            limits.kernel_max_threads_per_block
        );
        return;
    }

    // The two ends of the work-group axis, both measured from the card.
    //
    // The floor is the smallest launch that puts a resident block on every
    // multiprocessor: below it some SMs have no work and the hashrate describes
    // the launch being too small rather than the shape. The ceiling is what the
    // FREE device memory holds at the largest unit_size the grid explores, since
    // a batch needs 36 bytes a nonce; it is then also capped by the operator's
    // own [gpu] work_groups, exactly as the OpenCL path is capped by the device
    // work-group count it was opened with.
    let min_wg = limits.work_groups_that_fill_the_card();
    let memory_wg = limits.max_work_groups_for(CUDA_MAX_UNIT_SIZE, CUDA_TUNE_VRAM_SHARE);
    let max_wg = memory_wg.min(cnf.workgroups.max(min_wg)).max(min_wg);
    wlogln!(
        "[autotune] work-group axis {min_wg}..{max_wg}: {min_wg} is one resident block on each of \
         the {} multiprocessors, {memory_wg} is what {:.0}% of the {:.1} GiB free fits at \
         unit_size {CUDA_MAX_UNIT_SIZE} ({} bytes a nonce), and [gpu] work_groups = {} caps it",
        limits.device.multiprocessor_count,
        CUDA_TUNE_VRAM_SHARE * 100.0,
        limits.free_global_mem as f64 / (1024.0 * 1024.0 * 1024.0),
        x16rs_cuda::DEVICE_BYTES_PER_NONCE,
        cnf.workgroups,
    );

    // nvidia-smi's `-i` index and the CUDA device index are the same ordering on
    // a default setup, so the sensor follows the card being tuned unless the
    // operator named a different one in [efficiency] thermal_gpu_index.
    let sensor_index = if cnf.efficiency.thermal_gpu_index != 0 {
        cnf.efficiency.thermal_gpu_index
    } else {
        cnf.cudadevice.max(0) as u32
    };

    let request = crate::autotune16::TuneRequest {
        target: crate::autotune16::TuneTarget::Cuda {
            device_index: cnf.cudadevice,
        },
        local_size,
        min_work_groups: min_wg,
        max_work_groups: max_wg,
        max_unit_size: CUDA_MAX_UNIT_SIZE,
        vendor: crate::gpu_arch::GpuVendor::Nvidia,
        mode: cnf.efficiency.mode,
        budget_seconds: cnf.efficiency.benchmark_seconds.max(30) as u64,
        economics: autotune_economics(cnf),
        estimated_watts: cnf.efficiency.estimate_gpu_watts(&cnf.gpu_profile),
        thermal_file: cnf.efficiency.thermal_file.clone(),
        gpu_index: sensor_index,
        oracle_threads: autotune_oracle_threads(),
        headers: AUTOTUNE_CORPUS_HEADERS,
        proof_thresholds: AUTOTUNE_PROOF_THRESHOLDS,
        max_temp_c: (cnf.efficiency.max_temp_c > 0).then_some(cnf.efficiency.max_temp_c as f32),
    };

    match crate::autotune16::tune(&request) {
        Ok(outcome) => finish_tune(config_path, &request, &outcome),
        Err(error) => {
            wlogln!("[autotune] REJECTED: {error}");
            wlogln!("[autotune] Config unchanged.");
        }
    }
}

/// The largest `unit_size` the NVIDIA grid explores.
///
/// 128 rather than the 192 the RDNA4 path allows, and the reason is a
/// measurement rather than caution. On a Tesla T4 at repeat 16, work_groups 256
/// and local_size 256: unit_size 64 gave 7.54 MH/s, 96 gave 7.19 and 128 gave
/// 7.06. That is the REVERSE of the RX 9070 XT, where the kernel is latency
/// bound and 192 beats 64 by about 9%, and the reason is that the T4 sat at 66
/// to 67 W against a 70 W cap: a bigger batch cannot buy more work on a card
/// already at its power limit. So on this vendor the optimum is at the SMALL end
/// and the grid's job is to bracket it, which 32..128 does from both sides.
///
/// It is also what `ArchLimits::max_unit_size()` gives every non-gfx1201 slug,
/// so a shape written by this tune stays inside the range the rest of the config
/// and the panel already understand.
#[cfg(feature = "cuda")]
const CUDA_MAX_UNIT_SIZE: u32 = 128;

/// Share of FREE device memory the CUDA candidate grid may size itself to.
///
/// Not 1.0 and not close to it: cudaMalloc needs contiguous space, a display
/// driver on the same card takes memory `cudaMemGetInfo` has already counted as
/// free, and the tuner opens the winner's own miner for the soak while the
/// sweep's allocation is still being released. A grid sized to the last free
/// byte would spend the sweep watching allocations fail on exactly the largest
/// shapes, which are the ones the latency ceiling was going to drop anyway.
#[cfg(feature = "cuda")]
const CUDA_TUNE_VRAM_SHARE: f64 = 0.55;

#[cfg(not(feature = "ocl"))]
fn run_opencl_benchmark(cnf: &PoWorkConf, config_path: &str) {
    let _ = (cnf, config_path);
    wlogln!(
        "[autotune] This config has [gpu] use_opencl = true, but this binary was built without the \
         OpenCL backend. Rebuild with `cargo build --release --features ocl`. Config unchanged."
    );
}

#[cfg(feature = "ocl")]
fn run_opencl_benchmark(cnf: &PoWorkConf, config_path: &str) {
    {
        let scan = crate::opencl_diag::scan_opencl();
        // One probe open, purely to learn this device's hard limits. Everything
        // measured afterwards happens inside the tuner, which opens the device
        // itself at the shapes it needs.
        let probe = initialize_opencl(
            false,
            &cnf.opencldir,
            &cnf.platformid,
            &cnf.deviceids,
            &cnf.workgroups,
            &cnf.localsize,
            &cnf.unitsize.max(32),
            Some(&scan),
            false,
        );
        if probe.is_empty() {
            wlogln!(
                "[autotune] No OpenCL device could be opened (platform {}, device_ids '{}', \
                 kernels from '{}'). Nothing was measured and the config is unchanged.",
                cnf.platformid,
                cnf.deviceids,
                cnf.opencldir,
            );
            return;
        }
        if probe.len() != 1 {
            wlogln!(
                "[autotune] Auto Tune requires exactly one OpenCL device, but detected {}. The \
                 config has one shared work_groups/unit_size pair, so multi-GPU tuning would be \
                 ambiguous. Set [gpu] device_ids to one device and tune each GPU separately; \
                 config unchanged.",
                probe.len()
            );
            return;
        }
        let limits = crate::gpu_arch::ArchLimits::for_slug(&probe[0].arch_slug);
        let min_wg = limits.panel_min_wg.min(probe[0].workgroups);
        let max_wg = probe[0].workgroups;
        let max_us = limits.max_unit_size().max(32);
        let vendor = probe[0].vendor;
        drop(probe);

        let request = crate::autotune16::TuneRequest {
            target: crate::autotune16::TuneTarget::OpenCl {
                opencl_dir: cnf.opencldir.clone(),
                platform: cnf.platformid,
                device_ids: cnf.deviceids.clone(),
            },
            local_size: cnf.localsize,
            min_work_groups: min_wg,
            max_work_groups: max_wg,
            max_unit_size: max_us,
            vendor,
            mode: cnf.efficiency.mode,
            budget_seconds: cnf.efficiency.benchmark_seconds.max(30) as u64,
            economics: autotune_economics(cnf),
            estimated_watts: cnf.efficiency.estimate_gpu_watts(&cnf.gpu_profile),
            thermal_file: cnf.efficiency.thermal_file.clone(),
            gpu_index: cnf.efficiency.thermal_gpu_index,
            oracle_threads: autotune_oracle_threads(),
            headers: AUTOTUNE_CORPUS_HEADERS,
            proof_thresholds: AUTOTUNE_PROOF_THRESHOLDS,
            max_temp_c: (cnf.efficiency.max_temp_c > 0)
                .then_some(cnf.efficiency.max_temp_c as f32),
        };

        match crate::autotune16::tune(&request) {
            Ok(outcome) => finish_tune(config_path, &request, &outcome),
            Err(error) => {
                wlogln!("[autotune] REJECTED: {error}");
                wlogln!("[autotune] Config unchanged.");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact `[gpu]` section `scripts/mining-nvidia/INSTALL-CUDA-CONFIG.bat`
    /// installs, kept here verbatim so this test tracks what a real NVIDIA
    /// operator runs rather than a config invented for a test.
    const CUDA_INI: &str = "\
connect = 127.0.0.1:8080
supervene = 4
nonce_max = 4294967295
notice_wait = 45

[efficiency]
mode = profit
power_cost_kwh = 0.15
gpu_watts = 0
benchmark_seconds = 90

[gpu]
use_cuda = true
use_opencl = false
cuda_device = 0
cpu_assist = true
work_groups = 131072
local_size = 256
unit_size = 8
debug = 0
";

    fn temp_ini(tag: &str, body: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hacash-{tag}-{}-{}.config.ini",
            std::process::id(),
            id
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Auto Tune on a CUDA rig is routed to the CUDA tuner, and a build that has
    /// no CUDA backend says exactly that and touches nothing.
    ///
    /// This is the config the NVIDIA install script writes. The test suite runs
    /// without `--features cuda`, so what it pins here is the honest refusal: no
    /// device is opened, no OpenCL shape is measured, and the file is
    /// byte-identical afterwards. `benchmark_seconds` therefore stays at 90,
    /// which is what the panel reads back as "the benchmark did not produce a
    /// valid profile", rather than a config quietly filled with numbers measured
    /// on a backend this rig does not use.
    #[test]
    fn a_cuda_config_is_routed_to_cuda_and_a_build_without_it_leaves_the_file_untouched() {
        let path = temp_ini("cuda-autotune-route", CUDA_INI);
        let cnf = PoWorkConf::new(&sys::load_config_path(&path));
        assert!(cnf.usecuda, "the shipped CUDA config selects CUDA");
        assert!(!cnf.useopencl, "the shipped CUDA config leaves OpenCL off");
        assert_eq!(cnf.efficiency.benchmark_seconds, 90);
        assert_eq!(
            tuning_backend(&cnf),
            Some("cuda"),
            "a CUDA config must be measured on CUDA, never on OpenCL"
        );

        run_block_mining_benchmark(&cnf, path.to_str().unwrap());

        // Byte-identical whichever way the build went: with no CUDA feature
        // nothing could be measured, and with one nothing is written until a
        // soak settles.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CUDA_INI);
        let _ = std::fs::remove_file(&path);
    }

    /// The routing follows `use_cuda` first, and `use_opencl` only after it.
    ///
    /// This is the ordering `build_gpu_backends` uses to decide what the MINER
    /// runs, and the tuner has to agree with it: the next test shows what
    /// disagreeing costs.
    #[test]
    fn the_tuner_measures_whichever_backend_the_miner_will_run() {
        let opencl_ini = CUDA_INI
            .replace("use_cuda = true", "use_cuda = false")
            .replace("use_opencl = false", "use_opencl = true");
        let path = temp_ini("opencl-autotune-route", &opencl_ini);
        let cnf = PoWorkConf::new(&sys::load_config_path(&path));
        assert!(!cnf.usecuda && cnf.useopencl);
        assert_eq!(tuning_backend(&cnf), Some("opencl"));
        let _ = std::fs::remove_file(&path);

        // Both flags on: CUDA still wins, because `build_gpu_backends` prefers
        // CUDA and the shape a tune writes is the one the CUDA miner is then
        // built from.
        let both = CUDA_INI.replace("use_opencl = false", "use_opencl = true");
        let path = temp_ini("both-backends-autotune", &both);
        let cnf = PoWorkConf::new(&sys::load_config_path(&path));
        assert!(cnf.usecuda && cnf.useopencl);
        assert_eq!(
            tuning_backend(&cnf),
            Some("cuda"),
            "with both backends enabled the miner runs CUDA, so the tune must measure CUDA"
        );
        run_block_mining_benchmark(&cnf, path.to_str().unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), both);
        let _ = std::fs::remove_file(&path);

        // Neither: nothing to measure, and nothing written.
        let neither = CUDA_INI.replace("use_cuda = true", "use_cuda = false");
        let path = temp_ini("no-backend-autotune", &neither);
        let cnf = PoWorkConf::new(&sys::load_config_path(&path));
        assert_eq!(tuning_backend(&cnf), None);
        run_block_mining_benchmark(&cnf, path.to_str().unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), neither);
        let _ = std::fs::remove_file(&path);
    }

    /// Why the CUDA refusal has to come before the OpenCL gate.
    ///
    /// `build_gpu_backends` prefers CUDA (`if cnf.usecuda { .. } else if
    /// cnf.useopencl { .. }`) and hands `cnf.workgroups` / `cnf.unitsize` to
    /// `initialize_cuda`, while `apply_benchmark_pick` writes exactly those two
    /// numbers. So a tune that ran on OpenCL with `use_cuda = true` still in the
    /// file would land its winner in the numbers the CUDA miner is built from.
    ///
    /// The magnitudes below are the point: the shipped CUDA starting shape is
    /// 131072 x 256 x 8, and the OpenCL tuner's search space tops out at a few
    /// thousand work groups with unit_size in {32, 64, 128, 192}. This is not a
    /// translation between backends, it is an overwrite, and it is what
    /// `cuda_backend_refusal` now prevents by refusing on `use_cuda` rather than
    /// letting `use_opencl` alone decide.
    #[test]
    fn an_opencl_tune_result_lands_in_the_two_numbers_the_cuda_miner_is_built_from() {
        let both = CUDA_INI.replace("use_opencl = false", "use_opencl = true");
        let path = temp_ini("cuda-autotune-crossover", &both);

        let before = PoWorkConf::new(&sys::load_config_path(&path));
        assert!(before.usecuda && before.useopencl);
        assert_eq!((before.workgroups, before.unitsize), (131072, 8));

        // A plausible winner from an OpenCL sweep on an RTX 4070-class card.
        apply_benchmark_pick(
            path.to_str().unwrap(),
            &BenchmarkPick {
                profile: "nvidia_max".to_string(),
                workgroups: 1536,
                unitsize: 128,
            },
        )
        .unwrap();

        let after = PoWorkConf::new(&sys::load_config_path(&path));
        assert!(
            after.usecuda,
            "the tune did not turn CUDA off; the next run still mines on CUDA"
        );
        assert_eq!(
            (after.workgroups, after.unitsize),
            (1536, 128),
            "the OpenCL winner is now what initialize_cuda(cnf.cudadevice, cnf.workgroups, cnf.unitsize) is given"
        );
        assert_eq!(after.efficiency.benchmark_seconds, 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_upstream_stale_answer_is_recognized_on_both_miner_endpoints() {
        // Exact bodies the pool bridge returns during an upstream node outage.
        let pending = serde_json::json!({"err": "upstream stale; work is not being refreshed"});
        assert_eq!(
            upstream_stale_reason(&pending),
            Some("upstream stale; work is not being refreshed")
        );
        let notice = serde_json::json!({
            "err": "upstream stale; work is not being refreshed",
            "height": 1234
        });
        assert!(upstream_stale_reason(&notice).is_some());
    }

    #[test]
    fn an_unknown_or_missing_error_body_is_not_treated_as_stale() {
        // Anything else must stay an ordinary transient error, and no shape may
        // panic the miner.
        assert_eq!(upstream_stale_reason(&serde_json::json!({})), None);
        assert_eq!(upstream_stale_reason(&serde_json::json!({"err": ""})), None);
        assert_eq!(
            upstream_stale_reason(
                &serde_json::json!({"err": "no job yet; wait for upstream fullnode"})
            ),
            None
        );
        assert_eq!(upstream_stale_reason(&serde_json::json!({"err": 7})), None);
        assert_eq!(
            upstream_stale_reason(&serde_json::json!({"err": {"nested": true}})),
            None
        );
        assert_eq!(upstream_stale_reason(&serde_json::json!(null)), None);
        assert_eq!(
            upstream_stale_reason(&serde_json::json!({"height": 9, "block_intro": "00"})),
            None
        );
    }

    #[test]
    fn the_notice_cycle_has_an_anti_spin_floor_that_never_delays_new_work() {
        // An upstream that answers the notice immediately must not be hammered:
        // the cycle costs a pending request plus a notice request, so hold it to a
        // minimum period instead of running it as fast as the network allows.
        assert_eq!(
            notice_cycle_backoff(Duration::from_millis(5), false),
            NOTICE_CYCLE_MIN - Duration::from_millis(5)
        );
        assert_eq!(
            notice_cycle_backoff(Duration::ZERO, false),
            NOTICE_CYCLE_MIN
        );
        // A cooperating upstream already spent the whole notice window, so the
        // floor costs it nothing.
        assert_eq!(
            notice_cycle_backoff(Duration::from_secs(45), false),
            Duration::ZERO
        );
        // New work is a payout waiting to be mined and is never delayed.
        assert_eq!(
            notice_cycle_backoff(Duration::from_millis(5), true),
            Duration::ZERO
        );
    }

    #[test]
    fn notice_long_poll_is_capped_when_a_stop_flag_can_arrive() {
        assert_eq!(notice_poll_wait(45, true), NOTICE_POLL_WAIT_WITH_STOP);
        assert_eq!(notice_poll_wait(300, true), NOTICE_POLL_WAIT_WITH_STOP);
        assert_eq!(notice_poll_wait(1, true), 1);
        assert_eq!(notice_poll_wait(45, false), 45);
        assert_eq!(notice_poll_wait(300, false), 300);
    }

    #[test]
    fn cpu_group_mining_result_matches_manual_scan() {
        use field::Serialize;
        use protocol::block::BlockIntro;

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
