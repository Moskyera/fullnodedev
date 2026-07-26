//! Block PoW miner backends, batch dispatch, and result aggregation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, RwLock, mpsc};

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
/// How many recent heights the per-template submit gate remembers. A template
/// this far behind the tip can no longer be submitted anywhere, so its
/// bookkeeping is dropped: the gate must never grow for the life of the process.
const SUBMIT_GATE_HEIGHT_WINDOW: u64 = 8;
/// Hard ceiling on gate entries, so a burst of same-height reorgs cannot grow the
/// map even while the height itself stands still.
const SUBMIT_GATE_MAX_TEMPLATES: usize = 64;
/// How long one template is left alone after a pool answered `kind:"busy"`.
///
/// "Busy" means the pool is shedding load (HBIT refuses a share once it is
/// already holding its cap for that height), so the very next winner would be
/// refused too and hammering it makes the overload worse. Two seconds is an order
/// of magnitude longer than the 123 ms result drain tick, so the drain cadence
/// cannot defeat the back-off, and it is a tiny fraction of the 300 s target block
/// time, so a full network block found while the pool was busy is retried long
/// before the height can be lost. The template stays ALIVE throughout: this only
/// spaces the retries out.
const POOL_BUSY_COOLDOWN: Duration = Duration::from_secs(2);
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
/// Bumped on EVERY template install, including a mere re-serialization of the
/// same job. The submit gate uses it to tell "the upstream served us this exact
/// job again" from "nothing moved", which is the difference between a template
/// the upstream still treats as live work and one it has consumed.
static MINING_TEMPLATE_INSTALL_SEQ: AtomicU64 = AtomicU64::new(0);
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
    /// Generation of this job. Bumped ONLY when the job really changed (a
    /// different height, or the same height on a different parent). It is stored
    /// inside the template so a worker reads height, epoch and the bytes it mines
    /// from ONE snapshot and can never mislabel its own batch.
    epoch: u64,
    target_hash: Hash,
    block_intro: BlockIntro,
    coinbase_tx: TransactionCoinbase,
    mkrl_list: Vec<Hash>,
}

#[derive(Clone, Default)]
pub(crate) struct BlockMiningResult {
    worker_id: usize,
    pub height: u64,
    /// Parent hash of the template this result was mined against. Only a template
    /// replaced at THIS height with a different parent makes the result dead
    /// work; the tip merely advancing does not (see `result_is_orphaned`).
    prevhash: Vec<u8>,
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

/// (height, parent hash) of the template currently installed, read from ONE
/// snapshot so the pair is always consistent. `None` when the state cannot be
/// read: nothing is treated as dead work in that case, because dropping a winner
/// costs a whole block reward while submitting a late one costs one HTTP
/// round-trip and a deterministic rejection.
fn live_template_identity() -> Option<(u64, Vec<u8>)> {
    let stuff = MINING_BLOCK_STUFF.read().ok()?;
    Some((stuff.height, stuff.block_intro.prevhash().to_vec()))
}

/// A finished result is dead work ONLY when the node replaced the template at the
/// very height it was mined for with one built on a different parent (a
/// same-height reorg). That old template is evicted from the node's cache, so the
/// solution can no longer be reconstructed there.
///
/// Every other case is still worth submitting, and the NODE is the authority on
/// staleness: `miner_success` looks the template up by the SUBMITTED height and
/// the node deliberately keeps several recent heights, so a solution found a
/// moment before the tip advanced is still accepted as a competing candidate.
fn result_is_orphaned(res: &BlockMiningResult, live: Option<&(u64, Vec<u8>)>) -> bool {
    let Some((live_hei, live_prevhash)) = live else {
        return false;
    };
    res.height == *live_hei
        && !res.prevhash.is_empty()
        && !live_prevhash.is_empty()
        && res.prevhash != *live_prevhash
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

/// What the UPSTREAM said about one submitted winner.
///
/// `poworker::push_block_mining_success` returns `()`, so the classification the
/// per-template gate needs (a definitive upstream verdict versus never having
/// reached the upstream at all) is made here, next to the gate that consumes it.
///
/// Two upstreams speak here and they answer in different dialects. A FULLNODE
/// replies `{"ret":0,"mining":"success"}` or `{"ret":1,"err":"<text>"}`, so its
/// meaning is carried by the error TEXT. A POOL replies `{"ret":0|1,"kind":"<word>"}`
/// and carries no `err` at all, so its meaning is carried by the KIND. Both are
/// classified below; reading only the fullnode dialect made every pool answer look
/// like "a rejection about this one submission".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubmitVerdict {
    /// `ret:0` / `"mining":"success"` (fullnode), or `ret:0` / `"kind":"block"`
    /// (pool: it relayed a full NETWORK block). The block was accepted, so the
    /// height is filled and the node dropped its pending entry for it: no later
    /// winner for this template can ever be taken.
    Accepted,
    /// `ret:0` / `"kind":"share"` (pool). A PPLNS share was CREDITED and the pool
    /// wants every further share for the same template. On mainnet the share
    /// target is orders of magnitude easier than the network target, so this is
    /// the OVERWHELMINGLY common pool answer and each one of them is paid work.
    /// It must never settle the template and must never count a later winner as
    /// redundant, or the miner silently stops submitting the shares it is paid for.
    ShareCredited,
    /// The upstream answered, and the answer proves the TEMPLATE is gone or
    /// unusable: the fullnode's "pending block height N not found", "pending block
    /// not ready" or a difficulty check failure (it rebuilt the hash from a
    /// template that is not the one we mined), or a pool's `"kind":"stale"`, which
    /// says outright that it has moved to another job. Later winners are dead too.
    TemplateGone,
    /// `ret:1` / `"kind":"busy"` (pool). The pool is shedding load, NOT refusing
    /// the template: it is still live work. Retry, but not immediately, so a miner
    /// finding hundreds of nonces a second does not hammer an upstream that just
    /// said it is overloaded (see `POOL_BUSY_COOLDOWN`).
    UpstreamBusy,
    /// `ret:1` / `"kind":"duplicate"` (pool). This exact (height, coinbase_nonce,
    /// block_nonce) was already credited. Nothing is proved about the template and
    /// a DIFFERENT nonce is still wanted, so it stays open; resending this same
    /// submission is the one thing that cannot help.
    DuplicateSubmission,
    /// The upstream answered, but about THIS submission only (a fullnode error
    /// text that is not one of the template rules, or a pool's `"kind":"invalid"`).
    /// Nothing is proved about the template, so a later winner still deserves its
    /// own attempt.
    SubmissionRejected,
    /// No upstream verdict at all: connection refused, timeout, or a body carrying
    /// no `ret` (proxy error page, truncated reply). A network hiccup must never
    /// suppress a height.
    NoNodeVerdict,
}

impl SubmitVerdict {
    /// True when the verdict proves nothing more can be won on this template.
    fn ends_template(self) -> bool {
        matches!(self, SubmitVerdict::Accepted | SubmitVerdict::TemplateGone)
    }

    /// True when the upstream PAID for this winner and wants the next one. The
    /// gate stops suppressing redundant winners for such a template: on a pool
    /// every further share is separately payable, so dropping one is lost income.
    fn credits_share(self) -> bool {
        matches!(self, SubmitVerdict::ShareCredited)
    }

    /// How long to leave this template alone before submitting for it again.
    /// Only an overloaded upstream asks for that; every other verdict is either
    /// final or immediately retryable.
    fn retry_cooldown(self) -> Option<Duration> {
        match self {
            SubmitVerdict::UpstreamBusy => Some(POOL_BUSY_COOLDOWN),
            _ => None,
        }
    }

    /// Wording for the single per-template operator line.
    fn outcome_text(self) -> &'static str {
        match self {
            SubmitVerdict::Accepted => "settled (accepted)",
            SubmitVerdict::ShareCredited => "retired with its last share credited",
            SubmitVerdict::TemplateGone => "settled (template gone or stale)",
            SubmitVerdict::UpstreamBusy => "retired while the upstream was busy",
            SubmitVerdict::DuplicateSubmission => "retired after a duplicate submission",
            SubmitVerdict::SubmissionRejected => "retired after a submission rejection",
            SubmitVerdict::NoNodeVerdict => "retired without a node verdict",
        }
    }
}

/// A node rejection that is about the TEMPLATE rather than about one submission.
/// Every text below means the node can no longer rebuild our block at all, so
/// every further winner for the same template would get the same answer.
fn node_error_ends_template(err: &str) -> bool {
    let err = err.to_ascii_lowercase();
    err.contains("not found")
        || err.contains("pending block not ready")
        || err.contains("difficulty check failed")
}

/// The POOL dialect of `/submit/miner/success`: `{"ret":0|1,"kind":"<word>"}`,
/// with no `err` text at all for most kinds (the pool's miner-API arm keeps only
/// `ret` and `kind`). `None` means no `kind` we know, so the fullnode rules below
/// decide instead.
///
/// The kinds come straight from the HBIT pool's `handle_submission`:
///   `block`     the pool relayed a full network block: the template is DEAD.
///   `share`     a PPLNS share was credited: the template is ALIVE and paying.
///   `stale`     the pool has moved to another job: DEAD.
///   `busy`      too many shares held for this height: ALIVE, back off.
///   `duplicate` this exact (height, coinbase_nonce, block_nonce) was seen: ALIVE.
///   `invalid`   this one submission is bad (bad nonce, above share target, or an
///               unknown worker): ALIVE.
fn pool_kind_verdict(ret: i64, kind: &str) -> Option<SubmitVerdict> {
    match kind {
        "block" if ret == 0 => Some(SubmitVerdict::Accepted),
        "share" if ret == 0 => Some(SubmitVerdict::ShareCredited),
        "stale" => Some(SubmitVerdict::TemplateGone),
        "busy" => Some(SubmitVerdict::UpstreamBusy),
        "duplicate" => Some(SubmitVerdict::DuplicateSubmission),
        "invalid" => Some(SubmitVerdict::SubmissionRejected),
        _ => None,
    }
}

/// Classify one `/submit/miner/success` reply, returning the verdict and the
/// upstream's reason text alongside it.
///
/// `None` means the body carries no numeric `ret` at all (proxy error page,
/// truncated or non-JSON body). That is NOT an upstream decision, so the caller
/// retries instead of treating a front-end hiccup as a rejection.
fn classify_submit_response(body: &str) -> Option<(SubmitVerdict, String)> {
    let parsed = serde_json::from_str::<JV>(body).ok()?;
    let ret = parsed["ret"].as_i64()?;
    // The fullnode answers with `err`; the pool relay wraps an unrecognized
    // upstream body as `msg`. Both carry the reason.
    let err = parsed["err"]
        .as_str()
        .or_else(|| parsed["msg"].as_str())
        .unwrap_or("")
        .to_string();
    // A pool states its meaning in `kind` and sends no error text, so this is read
    // first: falling through to the text rules classified every one of its answers
    // as a rejection of one submission, which re-opened a template the pool had
    // already declared stale and let every later winner pay for another round trip.
    let kind = parsed["kind"].as_str().unwrap_or("");
    if let Some(verdict) = pool_kind_verdict(ret, kind) {
        let reason = if err.is_empty() {
            format!("kind \"{kind}\"")
        } else {
            format!("kind \"{kind}\": {err}")
        };
        return Some((verdict, reason));
    }
    if ret == 0 {
        // An unmarked success keeps the meaning it has always had here: the
        // fullnode's own success carries `"mining":"success"`, and older or
        // proxied fullnode builds answer a bare `{"ret":0}` for the same thing.
        // A wrong settle here is self-healing anyway, because `maintain` re-opens
        // a settled template the upstream keeps serving.
        return Some((SubmitVerdict::Accepted, err));
    }
    let verdict = if node_error_ends_template(&err) {
        SubmitVerdict::TemplateGone
    } else {
        SubmitVerdict::SubmissionRejected
    };
    Some((verdict, err))
}

/// Submit one winning result and report what the node said about it.
///
/// Same wire behaviour as `poworker::push_block_mining_success` (same URL, same
/// five attempts, same operator banners); the difference is the return value.
/// The gate below cannot be driven safely without knowing a definitive node
/// verdict from a transport failure, and that function returns `()`.
fn submit_block_mining_success(cnf: &PoWorkConf, success: &BlockMiningResult) -> SubmitVerdict {
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
    let mut verdict = SubmitVerdict::NoNodeVerdict;
    let mut last = String::new();
    for attempt in 1..=MAX_SUBMIT_ATTEMPTS {
        match crate::rpc_http::get_text(&super::HTTP_CLIENT, &urlapi_success, &cnf.api_token, None)
        {
            Ok(body) => {
                last = body.clone();
                match classify_submit_response(&body) {
                    Some((node_verdict, err)) => {
                        verdict = node_verdict;
                        match node_verdict {
                            // A taken block and a credited pool share are both
                            // normal PAID outcomes, not refusals.
                            SubmitVerdict::Accepted | SubmitVerdict::ShareCredited => {}
                            // Deterministic upstream rejection (stale template,
                            // duplicate, busy, invalid): resending this exact
                            // submission cannot help, so stop attempting.
                            _ => {
                                println!(
                                    "[submit] node rejected height {}: {}",
                                    success.height, err
                                );
                            }
                        }
                        break;
                    }
                    None => {
                        // HTTP 200 but no parseable `ret` (proxy/load-balancer
                        // error page, truncated or non-JSON body). This is NOT a
                        // node decision, so treat it as transient and retry: a
                        // winning block is not discarded on a front-end hiccup.
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
    match verdict {
        SubmitVerdict::Accepted => {
            println!(
                "\n\n████████████████ [MINING SUCCESS] Find a block height {},\n██ hash {} to submit.",
                success.height,
                success.result_hash.to_hex()
            );
        }
        // Routine pool traffic, not a rig fault: a credited share is income and a
        // busy or duplicate answer is the pool's own bookkeeping. Raising the
        // "check the node/connection" alarm for these would cry wolf on every
        // single share a pool miner earns.
        SubmitVerdict::ShareCredited => {
            println!(
                "[submit] pool credited a share at height {} (hash {}).",
                success.height,
                success.result_hash.to_hex()
            );
        }
        SubmitVerdict::UpstreamBusy => {
            println!(
                "[submit] pool is busy at height {}: pausing submits for it for {}s.",
                success.height,
                POOL_BUSY_COOLDOWN.as_secs()
            );
        }
        SubmitVerdict::DuplicateSubmission => {
            println!(
                "[submit] height {} was already credited for this nonce (hash {}).",
                success.height,
                success.result_hash.to_hex()
            );
        }
        _ => {
            println!(
                "\n\n████████████████ [MINING SUBMIT FAILED] block height {} was NOT confirmed accepted\n██ after {} attempts (hash {}). Check the node/connection.",
                success.height,
                MAX_SUBMIT_ATTEMPTS,
                success.result_hash.to_hex()
            );
        }
    }
    println!("▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔");
    verdict
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TemplateSubmitState {
    /// Nothing outstanding: the next winner for this template goes to the node.
    NotSubmitted,
    /// A winner for this template is with the submit thread right now.
    InFlight,
    /// The node gave a definitive verdict: nothing more can be won here.
    Settled,
}

struct TemplateGateEntry {
    state: TemplateSubmitState,
    /// Winners dropped without any HTTP call, because one was already in flight
    /// for this template or the node had already settled it.
    suppressed: u64,
    /// The upstream answered `kind:"share"` for this template, so it CREDITS every
    /// winner instead of holding one block. Redundant-winner suppression stops for
    /// it: each further share is separately payable work, and dropping one is
    /// money the miner earned and never gets.
    share_paying: bool,
    /// Earliest instant a further winner for this template may be submitted. Set
    /// only by a `busy` answer, so an overloaded pool is not hammered by a rig
    /// finding hundreds of nonces a second.
    cooldown_until: Option<Instant>,
    outcome: &'static str,
    /// Template install counter as of the moment this template settled. If the
    /// upstream serves the very same job again afterwards it has plainly not
    /// consumed it, and the gate re-opens (see `maintain`).
    settled_at_install: u64,
    /// The single operator line for this template has already been printed.
    reported: bool,
}

impl TemplateGateEntry {
    fn new() -> TemplateGateEntry {
        TemplateGateEntry {
            state: TemplateSubmitState::NotSubmitted,
            suppressed: 0,
            share_paying: false,
            cooldown_until: None,
            outcome: SubmitVerdict::NoNodeVerdict.outcome_text(),
            settled_at_install: 0,
            reported: false,
        }
    }
}

/// Per-template submit gate, keyed by (height, parent hash) - the same identity a
/// `BlockMiningResult` already carries.
///
/// At a low difficulty a fast GPU finds MANY nonces meeting one template's target,
/// but a height holds exactly ONE block: `mint::api::miner_success` drops the
/// pending entry for that height the moment it accepts a block, so every later
/// winner for the same template can only come back as "pending block height N not
/// found". Submitting them anyway costs one blocking HTTP round trip each, which
/// is what filled the submit queue, then the result channel, then blocked the
/// workers and stopped the GPU.
///
/// So the FIRST winner for a template is always submitted (that is the payout,
/// and it must never regress), while later winners for the SAME template are
/// dropped only once the node has taken it or proved it gone.
///
/// None of that holds against a POOL, which credits a PPLNS share for EVERY
/// winner instead of holding one block, so the gate stops suppressing as soon as
/// the upstream proves it pays that way.
#[derive(Default)]
struct SubmitGate {
    templates: HashMap<(u64, Vec<u8>), TemplateGateEntry>,
    /// The upstream has credited a share at least once, so it is a pool and it
    /// pays per winner. Latched for the whole session and inherited by every new
    /// template: a fresh template starting out "solo" again would suppress the
    /// burst of shares found before its first verdict comes back, and those are
    /// earned income. The rpcaddr cannot change while the miner runs, and a
    /// definitive verdict still settles any template, so an upstream that later
    /// behaves like a fullnode is still gated from its first answer onwards.
    upstream_credits_shares: bool,
}

type SubmitGateHandle = Arc<Mutex<SubmitGate>>;

fn template_key(res: &BlockMiningResult) -> (u64, Vec<u8>) {
    (res.height, res.prevhash.clone())
}

impl SubmitGate {
    /// True when this winner must go to the upstream. False means it is a
    /// redundant winner for a template already in flight or already settled, so
    /// dropping it costs nothing; it is counted for the one summary line.
    fn admit(&mut self, res: &BlockMiningResult) -> bool {
        self.admit_at(res, Instant::now())
    }

    /// `admit` with the clock passed in, so the busy back-off is testable.
    fn admit_at(&mut self, res: &BlockMiningResult, now: Instant) -> bool {
        let pays_per_winner = self.upstream_credits_shares;
        let entry = self
            .templates
            .entry(template_key(res))
            .or_insert_with(TemplateGateEntry::new);
        if entry.state == TemplateSubmitState::Settled {
            entry.suppressed = entry.suppressed.saturating_add(1);
            return false;
        }
        if entry.cooldown_until.is_some_and(|until| now < until) {
            // The upstream said it is overloaded moments ago. Submitting again
            // right now would only be refused again and add to the load.
            entry.suppressed = entry.suppressed.saturating_add(1);
            return false;
        }
        if entry.share_paying || pays_per_winner {
            // This upstream PAYS per winner, so there is no such thing as a
            // redundant one: a height holds a single block, but a pool credits
            // every share against the same template. Suppressing here is the
            // miner refusing its own income.
            entry.state = TemplateSubmitState::InFlight;
            return true;
        }
        if entry.state == TemplateSubmitState::NotSubmitted {
            entry.state = TemplateSubmitState::InFlight;
            return true;
        }
        entry.suppressed = entry.suppressed.saturating_add(1);
        false
    }

    /// Record what the upstream said about the winner that was in flight.
    fn record_verdict(
        &mut self,
        res: &BlockMiningResult,
        verdict: SubmitVerdict,
        install_seq: u64,
    ) {
        self.record_verdict_at(res, verdict, install_seq, Instant::now());
    }

    /// `record_verdict` with the clock passed in, so the busy back-off is testable.
    fn record_verdict_at(
        &mut self,
        res: &BlockMiningResult,
        verdict: SubmitVerdict,
        install_seq: u64,
        now: Instant,
    ) {
        if verdict.credits_share() {
            // Latched, and outside the entry lookup so it survives even if this
            // template has already been pruned: an upstream that credited one
            // share pays for the rest of them too, and the answer to the NEXT
            // winner cannot arrive before that winner has to be admitted.
            self.upstream_credits_shares = true;
        }
        let Some(entry) = self.templates.get_mut(&template_key(res)) else {
            // Already pruned: the template is far behind the tip, so there is
            // nothing left to gate.
            return;
        };
        if verdict.credits_share() {
            entry.share_paying = true;
        }
        // Any answer that is not "busy" clears the back-off, so a pool that has
        // recovered is used again at once.
        entry.cooldown_until = verdict.retry_cooldown().map(|wait| now + wait);
        if verdict.ends_template() {
            entry.state = TemplateSubmitState::Settled;
            entry.outcome = verdict.outcome_text();
            entry.settled_at_install = install_seq;
        } else if entry.state != TemplateSubmitState::Settled {
            // Nothing proves this template is dead, so re-open it: a network
            // hiccup, a credited share, a busy or duplicate answer, or a rejection
            // about one submission, must never permanently suppress a height.
            // A template that IS settled keeps that outcome: settling is
            // definitive, and on a share-paying template a second submission can
            // still be in flight when the first one settles it.
            entry.state = TemplateSubmitState::NotSubmitted;
            entry.outcome = verdict.outcome_text();
        }
    }

    /// Re-open a settled template the upstream is STILL serving, print the ONE
    /// line per template the operator gets, then forget templates too old to
    /// submit anywhere. A line is emitted only once the miner has moved off that
    /// template, so the suppressed count in it is the final one.
    fn maintain(&mut self, live: Option<&(u64, Vec<u8>)>, live_height: u64, install_seq: u64) {
        for (key, entry) in self.templates.iter_mut() {
            let still_live =
                live.is_some_and(|(hei, prevhash)| *hei == key.0 && *prevhash == key.1);
            if still_live {
                // A fullnode that took our block moves on, so it never serves that
                // job again. An upstream that DOES serve it again has not consumed
                // it (a pool grading work at one height, or a block reorged back
                // out), and our own bookkeeping must not be what silences a live
                // height. Re-opening costs at most one submit per template refresh.
                if entry.state == TemplateSubmitState::Settled
                    && install_seq > entry.settled_at_install
                {
                    entry.state = TemplateSubmitState::NotSubmitted;
                }
                continue;
            }
            if entry.reported {
                continue;
            }
            // Nothing was dropped for this template, so there is nothing to
            // report: the submit path already printed its own outcome.
            if entry.suppressed > 0 {
                if entry.state == TemplateSubmitState::Settled {
                    println!(
                        "\n[Mining] height {} {}, suppressed {} redundant winners.",
                        key.0, entry.outcome, entry.suppressed
                    );
                } else {
                    // Never settled, so these were not provably dead: they were
                    // held back while a submission was in flight or while the
                    // upstream asked for a pause. Say so instead of calling them
                    // redundant.
                    println!(
                        "\n[Mining] height {} {}, held back {} further winners.",
                        key.0, entry.outcome, entry.suppressed
                    );
                }
            }
            entry.reported = true;
        }
        self.prune(live_height);
    }

    /// Bound the memory: keep only the last `SUBMIT_GATE_HEIGHT_WINDOW` heights,
    /// and never more than `SUBMIT_GATE_MAX_TEMPLATES` entries in total.
    fn prune(&mut self, live_height: u64) {
        if live_height > 0 {
            let floor = live_height.saturating_sub(SUBMIT_GATE_HEIGHT_WINDOW);
            self.templates.retain(|(hei, _), _| *hei >= floor);
        }
        while self.templates.len() > SUBMIT_GATE_MAX_TEMPLATES {
            let Some(oldest) = self.templates.keys().min().cloned() else {
                break;
            };
            self.templates.remove(&oldest);
        }
    }
}

/// A poisoned gate must never stop submissions: recover the state and carry on.
fn lock_gate(gate: &SubmitGateHandle) -> MutexGuard<'_, SubmitGate> {
    gate.lock().unwrap_or_else(|e| e.into_inner())
}

fn admit_for_submit(gate: &SubmitGateHandle, win: &BlockMiningResult) -> bool {
    lock_gate(gate).admit(win)
}

fn record_submit_verdict(gate: &SubmitGateHandle, win: &BlockMiningResult, verdict: SubmitVerdict) {
    let install_seq = MINING_TEMPLATE_INSTALL_SEQ.load(Relaxed);
    lock_gate(gate).record_verdict(win, verdict, install_seq);
}

/// Re-open, retire and prune templates. Runs on the result thread every drain
/// tick, so the gate cannot grow while mining continues.
fn maintain_submit_gate(gate: &SubmitGateHandle) {
    let live = live_template_identity();
    let live_height = MINING_BLOCK_HEIGHT.load(Relaxed);
    let install_seq = MINING_TEMPLATE_INSTALL_SEQ.load(Relaxed);
    lock_gate(gate).maintain(live.as_ref(), live_height, install_seq);
}

/// Queue a winning result for the submit thread.
///
/// With the gate above at most one winner per template is ever outstanding, so
/// the queue holds a handful of entries and can no longer fill in the redundant
/// winner case that used to jam the whole pipeline. The inline submit is kept
/// purely as a last resort (the submit thread is gone, i.e. shutdown), because a
/// dropped winner is lost money.
fn queue_block_mining_success(
    cnf: &PoWorkConf,
    submit_tx: &mpsc::SyncSender<Arc<BlockMiningResult>>,
    gate: &SubmitGateHandle,
    win: &Arc<BlockMiningResult>,
) {
    match submit_tx.try_send(win.clone()) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(win)) => {
            eprintln!(
                "[Mining] Submit queue full, submitting height {} inline.",
                win.height
            );
            submit_inline_without_verdict(cnf, gate, &win);
        }
        Err(mpsc::TrySendError::Disconnected(win)) => {
            submit_inline_without_verdict(cnf, gate, &win);
        }
    }
}

/// Last-resort submit on the calling thread. It uses the plain submitter, which
/// reports nothing back, so the template is re-opened instead of being left in
/// flight: no height may stay suppressed on the strength of a submit whose
/// outcome we never learned.
fn submit_inline_without_verdict(
    cnf: &PoWorkConf,
    gate: &SubmitGateHandle,
    win: &Arc<BlockMiningResult>,
) {
    super::push_block_mining_success(cnf, win);
    record_submit_verdict(gate, win, SubmitVerdict::NoNodeVerdict);
}

pub(crate) fn start_block_mining_workers(
    cnf: &PoWorkConf,
    stop_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> bool {
    let (res_tx, res_rx) = mpsc::sync_channel(RESULT_CHANNEL_CAPACITY);
    let miner_backends = build_miner_backends(cnf, &stop_flag);
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
    // One gate shared by the result thread (which admits winners) and the submit
    // thread (which reports the node verdict back).
    let submit_gate: SubmitGateHandle = Arc::new(Mutex::new(SubmitGate::default()));
    let cnf_submit = cnf.clone();
    let submit_thread_guard = cnf.runtime.track_mining_thread();
    let gate_submit = submit_gate.clone();
    spawn(move || {
        let _submit_thread_guard = submit_thread_guard;
        while let Ok(win) = submit_rx.recv() {
            // A panic inside the submit must not leave the template stuck "in
            // flight", which would suppress every later winner for it, so the
            // verdict starts as "no node verdict" (re-open) and is recorded
            // outside the firewall.
            let mut verdict = SubmitVerdict::NoNodeVerdict;
            guard_mining_iteration("block submit thread", || {
                verdict = submit_block_mining_success(&cnf_submit, &win);
            });
            record_submit_verdict(&gate_submit, &win, verdict);
        }
    });

    let cnf1 = cnf.clone();
    let worker_qty = miner_backends.len();
    let stop_flag_res = stop_flag.clone();
    let result_thread_guard = cnf.runtime.track_mining_thread();
    let gate_results = submit_gate;
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
                // Final drain: do not abandon target-meeting winners still in the
                // channel just because shutdown was requested. This is the only
                // other body on this thread that can submit, so it gets the same
                // panic firewall as the drain loop below.
                guard_mining_iteration("block shutdown drain", || {
                    drain_winners_for_shutdown(&mut rstx, &submit_tx, &gate_results);
                });
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
                    &gate_results,
                );
                // Retire settled templates (one operator line each) and keep the
                // gate bounded. This runs every tick, so it also happens while no
                // result at all is coming in.
                maintain_submit_gate(&gate_results);
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
    // JSON height drives x16rs repeat count; consensus uses the height embedded in
    // the intro. Reject mismatches so we never mine with the wrong repeat or
    // submit under a height the header does not claim.
    let intro_height = block_intro.height().uint();
    if height != intro_height {
        return Err(format!(
            "pending height {height} does not match block_intro height {intro_height}"
        ));
    }
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
        epoch: 0,
        target_hash,
        block_intro,
        coinbase_tx,
        mkrl_list,
    };
    install_block_mining_stuff(new_stuff)
}

/// Install a freshly fetched template.
///
/// The fullnode re-serializes `block_intro` on EVERY `/query/miner/pending`
/// request: it increments the served coinbase nonce, recomputes the merkle root
/// and re-serializes the intro, and the merkle root lives INSIDE the intro. So
/// the raw bytes differ on every poll even when the tip, the parent and the
/// transaction set are all unchanged. The miner replaces that merkle root itself
/// from its own coinbase nonce before hashing, so it is not part of the job at
/// all.
///
/// A job is therefore identified by (height, parent hash). Only a change of that
/// pair means workers must give up what they are doing, so only that bumps the
/// epoch. The refreshed bytes are installed either way. Bumping on every poll
/// used to void one finished batch per worker per poll, winners included.
fn install_block_mining_stuff(mut new_stuff: BlockMiningStuff) -> Result<(), String> {
    let height = new_stuff.height;
    let mut guard = MINING_BLOCK_STUFF
        .write()
        .map_err(|e| format!("mining state lock poisoned: {e}"))?;
    let job_changed =
        guard.height != height || guard.block_intro.prevhash() != new_stuff.block_intro.prevhash();
    // Keep the epoch inside the template so workers see the same generation the
    // bytes belong to. The atomic mirrors it for the cheap loop-exit check.
    new_stuff.epoch = if job_changed {
        MINING_BLOCK_EPOCH.fetch_add(1, Relaxed).saturating_add(1)
    } else {
        guard.epoch
    };
    *guard = new_stuff.into();
    MINING_BLOCK_HEIGHT.store(height, Relaxed);
    // Every install counts here, re-serializations included: the submit gate needs
    // to know the upstream handed us this job again, not whether it changed.
    MINING_TEMPLATE_INSTALL_SEQ.fetch_add(1, Relaxed);
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

/// Seconds to wait BEFORE each GPU init attempt after the first. A driver that is
/// still loading after a boot, a resume, or a TDR reset routinely needs tens of
/// seconds to become enumerable, and treating the very first probe as final turns
/// a self-clearing condition into a permanent outage on an unattended rig.
const GPU_INIT_RETRY_DELAYS_SECS: [u64; 4] = [5, 15, 30, 60];

/// How many GPU init attempts to make. Retrying only helps when the requested
/// backend is actually compiled in; a missing feature is deterministic, so waiting
/// two minutes to reach the same answer would only stall startup.
fn gpu_init_attempts(gpu_requested: bool, gpu_backend_compiled_in: bool) -> usize {
    if gpu_requested && gpu_backend_compiled_in {
        GPU_INIT_RETRY_DELAYS_SECS.len() + 1
    } else {
        1
    }
}

/// CPU-assist threads belong to ANY GPU rig, not only an OpenCL one. This decision
/// used to live inside the OpenCL arm, so a CUDA rig had no non-GPU backend at
/// all: once the CUDA session-disable latch quarantined the card, the whole rig
/// was left with a single bounded recovery thread and earned nothing.
fn cpu_assist_thread_count(cnf: &PoWorkConf, gpu_backends: usize) -> usize {
    if !cnf.cpu_assist || cnf.supervene == 0 || gpu_backends == 0 {
        return 0;
    }
    cnf.efficiency.spawn_supervene(cnf.supervene) as usize
}

/// Sleep, but wake as soon as a supervisor asks us to stop. Returns true when the
/// wait ended because of a stop request, so startup never holds shutdown.
fn sleep_unless_stopped(
    stop_flag: &Option<Arc<std::sync::atomic::AtomicBool>>,
    total: Duration,
) -> bool {
    let deadline = Instant::now() + total;
    loop {
        if super::should_stop(stop_flag) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(200)),
        );
    }
}

fn build_gpu_backends(cnf: &PoWorkConf) -> Vec<MinerBackend> {
    // Both arms below are cfg-gated, so a build with neither GPU feature enabled
    // never mutates this.
    #[allow(unused_mut)]
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
    }

    backends
}

fn build_miner_backends(
    cnf: &PoWorkConf,
    stop_flag: &Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Vec<MinerBackend> {
    let gpu_requested = cnf.usecuda || cnf.useopencl;
    let compiled_in =
        (cnf.usecuda && cfg!(feature = "cuda")) || (cnf.useopencl && cfg!(feature = "ocl"));
    let attempts = gpu_init_attempts(gpu_requested, compiled_in);
    let mut backends = Vec::new();
    for attempt in 1..=attempts {
        backends = build_gpu_backends(cnf);
        if !backends.is_empty() || attempt == attempts {
            break;
        }
        let wait = GPU_INIT_RETRY_DELAYS_SECS[attempt - 1];
        println!(
            "\n[Start] GPU not ready yet (attempt {attempt}/{attempts}). Waiting {wait}s and trying again - this is normal shortly after a boot, a resume, or a driver reset."
        );
        if sleep_unless_stopped(stop_flag, Duration::from_secs(wait)) {
            return Vec::new();
        }
    }

    // Any GPU rig gets CPU-assist threads, so a quarantined card still leaves real
    // work running instead of taking the whole rig to zero.
    let assist_threads = cpu_assist_thread_count(cnf, backends.len());
    if assist_threads > 0 {
        println!(
            "\n[Start] Create #{} Ryzen CPU assist threads (hybrid GPU+CPU, active={}).",
            assist_threads,
            cnf.runtime.active_cpu_assist.load(Relaxed)
        );
        for i in 0..assist_threads {
            backends.push(MinerBackend::Cpu {
                assist_idx: Some(i as u32),
            });
        }
    }

    if backends.is_empty() {
        if gpu_requested {
            eprintln!(
                "[Fatal] a GPU miner was requested but no usable GPU backend initialized after {attempts} attempt(s); refusing silent CPU fallback (you would pay for GPU power while mining slowly on the CPU). If the GPU works in other software, wait a minute and press Start again; otherwise check the driver/CUDA runtime, or set the backend to CPU."
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

    // Height, epoch and the bytes to mine all come from ONE snapshot. Reading them
    // separately let a template install land in between, which labelled the batch
    // with a generation it was not mined under.
    let stuff = match MINING_BLOCK_STUFF.read() {
        Ok(stuff) => stuff.clone(),
        Err(e) => {
            eprintln!("[Mining] Block state lock failed: {e}");
            return;
        }
    };
    let mining_hei = stuff.height;
    let mining_epoch = stuff.epoch;
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
    let height = mining_hei;
    let prevhash = stuff.block_intro.prevhash().to_vec();
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

        // A FINISHED batch is never thrown away, whatever the template did while it
        // ran. Those hashes were really computed: the sample keeps the hashrate
        // display honest, and any nonce that met the target is a payout the node
        // still accepts at the height it was mined for. Stopping the loop is the
        // job-switch response (see the re-check at the end of this loop); dropping
        // the result is not.
        let use_secs = ctn.elapsed().as_secs_f64();
        let mlres = BlockMiningResult {
            worker_id: thrid,
            height,
            prevhash: prevhash.clone(),
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

        // Any height change (including a reorg to a lower tip) or a real job change
        // means this template is done, so stop starting NEW batches and let the next
        // outer loop iteration pick the fresh one up. The batch that just finished
        // was already sent: a job switch ends the loop, it never voids mined work.
        let check_hei = MINING_BLOCK_HEIGHT.load(Relaxed);
        if check_hei != mining_hei || current_mining_epoch() != mining_epoch {
            return;
        }
    }
}

/// On shutdown, still submit any target-meeting results already in the channel.
///
/// Hand-off only: `queue_block_mining_success` falls back to a blocking inline
/// submit (up to five attempts at the 30 s RPC timeout) when the queue is full,
/// and holding shutdown on network I/O would look like a hung miner. The submit
/// thread drains whatever is queued before it exits, so a successful `try_send`
/// is still a real submission. The per-template gate applies here too, so a
/// backlog of redundant winners cannot turn shutdown into hundreds of doomed
/// round trips.
fn drain_winners_for_shutdown(
    result_ch_rx: &mut mpsc::Receiver<Arc<BlockMiningResult>>,
    submit_tx: &mpsc::SyncSender<Arc<BlockMiningResult>>,
    gate: &SubmitGateHandle,
) {
    let live_template = live_template_identity();
    while let Ok(res) = result_ch_rx.try_recv() {
        if result_meets_target(&res)
            && !result_is_orphaned(&res, live_template.as_ref())
            && admit_for_submit(gate, &res)
        {
            if let Err(e) = submit_tx.try_send(res.clone()) {
                eprintln!(
                    "[Mining] Shutdown could not queue a winning result at height {}: {e}",
                    res.height
                );
            }
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
    gate: &SubmitGateHandle,
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
    let live_template = live_template_identity();
    while let Ok(res) = result_ch_rx.try_recv() {
        deal_hei = res.height;
        // Prefer actual mined counts when recovery mined only a prefix of the plan.
        if res.gpu_nonce_space > 0 || res.cpu_nonce_space > 0 {
            total_nonce_space += res.gpu_nonce_space as u64 + res.cpu_nonce_space as u64;
            gpu_nonce_space += res.gpu_nonce_space as u64;
            cpu_nonce_space += res.cpu_nonce_space as u64;
        } else if res.is_gpu {
            total_nonce_space += res.nonce_space as u64;
            gpu_nonce_space += res.nonce_space as u64;
        } else {
            total_nonce_space += res.nonce_space as u64;
            cpu_nonce_space += res.nonce_space as u64;
        }
        rate_tracker.record_result(&res, now_ms);
        if hash_more_power(&res.result_hash, &most.result_hash) {
            most = res.clone();
        }
        // Submit everything that met its own target. The height is NOT a filter:
        // the node looks the template up by the submitted height and keeps several
        // recent ones, so a solution found just before the tip advanced is still
        // money. Only a same-height reorg (a different parent at this very height)
        // is genuinely unreconstructable, and only that is dropped.
        if result_meets_target(&res) && !result_is_orphaned(&res, live_template.as_ref()) {
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
        // Submit every distinct winner the node could still take. The gate drops
        // only the ones it can prove are dead: a second winner for a template the
        // node has already settled cannot be accepted at any price, because a
        // height holds exactly one block. Everything else still goes out.
        for w in &winners {
            if !admit_for_submit(gate, w) {
                continue;
            }
            queue_block_mining_success(cnf, submit_tx, gate, w);
        }
    } else if cnf.debug == 1 {
        // Debug mode exercises the submit path even without a genuine winner. It
        // goes through the same gate, so it submits once per template instead of
        // once per drain tick.
        if admit_for_submit(gate, &most) {
            queue_block_mining_success(cnf, submit_tx, gate, &most);
        }
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

    /// The installed template lives in process-global statics, so every test that
    /// installs one has to run alone.
    fn mining_state_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// One `/query/miner/pending` body. `mrklroot` is the field the fullnode
    /// rewrites on every single request, and the one the miner replaces itself.
    fn pending_template_json(height: u64, prevhash: u8, mrklroot: u8) -> JV {
        let mut intro = BlockIntro::default();
        intro.head.height = BlockHeight::from(height);
        intro.head.prevhash = Hash::from([prevhash; HASH_WIDTH]);
        intro.head.mrklroot = Hash::from([mrklroot; HASH_WIDTH]);
        serde_json::json!({
            "height": height,
            "target_hash": hex::encode([0x0fu8; HASH_WIDTH]),
            "block_intro": hex::encode(intro.serialize()),
            "coinbase_body": hex::encode(TransactionCoinbase::default().serialize()),
            "mkrl_modify_list": Vec::<String>::new(),
        })
    }

    fn installed_mrklroot() -> Vec<u8> {
        MINING_BLOCK_STUFF
            .read()
            .unwrap()
            .block_intro
            .mrklroot()
            .to_vec()
    }

    #[test]
    fn a_reserialized_template_is_installed_without_voiding_in_flight_work() {
        let _guard = mining_state_guard();
        // The fullnode increments the served coinbase nonce and recomputes the
        // merkle root on EVERY pending request, and the merkle root sits inside the
        // serialized intro. Calling that a new job bumped the epoch on every poll,
        // and every bump destroyed one finished batch per worker, winners included.
        set_pending_block_stuff(41, pending_template_json(41, 0x11, 0xa1)).unwrap();
        let installed_epoch = current_mining_epoch();

        set_pending_block_stuff(41, pending_template_json(41, 0x11, 0xa2)).unwrap();
        assert_eq!(
            current_mining_epoch(),
            installed_epoch,
            "a merely re-serialized template must not count as a new job"
        );
        assert_eq!(
            installed_mrklroot(),
            vec![0xa2u8; HASH_WIDTH],
            "the refreshed template must still be installed"
        );

        // A same-height reorg replaces the parent: that IS a new job.
        set_pending_block_stuff(41, pending_template_json(41, 0x22, 0xa3)).unwrap();
        assert_eq!(current_mining_epoch(), installed_epoch + 1);

        // So is the tip advancing.
        set_pending_block_stuff(42, pending_template_json(42, 0x33, 0xa4)).unwrap();
        assert_eq!(current_mining_epoch(), installed_epoch + 2);
        assert_eq!(current_mining_height(), 42);
    }

    #[test]
    fn a_cuda_rig_also_gets_cpu_assist_threads() {
        // The assist block used to live inside the OpenCL arm, so a CUDA rig had no
        // non-GPU backend at all: when the session-disable latch quarantined the
        // card, the whole rig produced nothing.
        let mut cnf = PoWorkConf::test_defaults("127.0.0.1:1".to_string(), 2, 16);
        cnf.cpu_assist = true;
        cnf.usecuda = true;
        cnf.useopencl = false;
        assert!(cpu_assist_thread_count(&cnf, 1) > 0);
        cnf.usecuda = false;
        cnf.useopencl = true;
        assert!(cpu_assist_thread_count(&cnf, 1) > 0);

        // No GPU backend means the CPU-only path below owns the thread count, and
        // an operator who turned the assist off still gets none.
        assert_eq!(cpu_assist_thread_count(&cnf, 0), 0);
        cnf.cpu_assist = false;
        assert_eq!(cpu_assist_thread_count(&cnf, 1), 0);
        cnf.cpu_assist = true;
        cnf.supervene = 0;
        assert_eq!(cpu_assist_thread_count(&cnf, 1), 0);
    }

    #[test]
    fn gpu_init_is_retried_before_it_is_called_fatal() {
        // A driver still loading after a boot or a TDR reset is self-clearing, so
        // one failed probe must not stop an unattended rig for good.
        assert_eq!(gpu_init_attempts(true, true), 5);
        // Nothing to wait for when the backend is not compiled in, or not wanted.
        assert_eq!(gpu_init_attempts(true, false), 1);
        assert_eq!(gpu_init_attempts(false, true), 1);
        // The waits stay bounded so a genuinely dead GPU is still reported.
        assert!(GPU_INIT_RETRY_DELAYS_SECS.iter().sum::<u64>() <= 120);
        assert!(GPU_INIT_RETRY_DELAYS_SECS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn a_startup_retry_wait_never_holds_shutdown() {
        let stop = Some(Arc::new(std::sync::atomic::AtomicBool::new(true)));
        let started = Instant::now();
        assert!(sleep_unless_stopped(&stop, Duration::from_secs(60)));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!sleep_unless_stopped(&None, Duration::from_millis(1)));
    }

    #[test]
    fn only_a_same_height_reorg_makes_a_solution_dead_work() {
        let mut res = BlockMiningResult::default();
        res.height = 100;
        res.prevhash = vec![0x11u8; HASH_WIDTH];
        res.target_hash = vec![0x0f; HASH_WIDTH];
        res.result_hash = vec![0x01; HASH_WIDTH];
        assert!(result_meets_target(&res));

        // Tip advanced past us: the node looks the template up by the SUBMITTED
        // height and keeps several recent heights, so this is still a payout.
        assert!(!result_is_orphaned(
            &res,
            Some(&(101u64, vec![0x99u8; HASH_WIDTH]))
        ));
        // Same job: obviously live.
        assert!(!result_is_orphaned(
            &res,
            Some(&(100u64, vec![0x11u8; HASH_WIDTH]))
        ));
        // Same height, different parent: the node evicted our template, so the
        // solution cannot be reconstructed there any more.
        assert!(result_is_orphaned(
            &res,
            Some(&(100u64, vec![0x22u8; HASH_WIDTH]))
        ));
        // Unknown live template must never cost a payout.
        assert!(!result_is_orphaned(&res, None));
    }

    #[test]
    fn a_winner_mined_just_before_the_tip_advanced_is_still_submitted() {
        let _guard = mining_state_guard();
        // Live template is height 501 on parent 0x22; the winner was mined at 500.
        set_pending_block_stuff(501, pending_template_json(501, 0x22, 0xb1)).unwrap();

        let cnf = PoWorkConf::test_defaults("127.0.0.1:1".to_string(), 1, 16);
        let (res_tx, mut res_rx) = mpsc::sync_channel::<Arc<BlockMiningResult>>(4);
        let (submit_tx, submit_rx) = mpsc::sync_channel::<Arc<BlockMiningResult>>(4);

        let mut win = BlockMiningResult::default();
        win.height = 500;
        win.prevhash = vec![0x11u8; HASH_WIDTH];
        win.nonce_space = 1;
        win.use_secs = 0.5;
        win.head_nonce = 77;
        win.coinbase_nonce = vec![0x05; HASH_WIDTH];
        win.target_hash = vec![0x0f; HASH_WIDTH];
        win.result_hash = vec![0x01; HASH_WIDTH];
        res_tx.send(Arc::new(win)).unwrap();

        let mut most_hash = vec![255u8; HASH_WIDTH];
        let mut tracker = HashrateTracker::default();
        deal_block_mining_results(
            &cnf,
            &mut most_hash,
            &mut res_rx,
            1,
            &mut tracker,
            1,
            &submit_tx,
            &test_gate(),
        );

        let submitted = submit_rx
            .try_recv()
            .expect("a solution found a moment before the tip advanced is still accepted by the node at its own height and must be submitted");
        assert_eq!(submitted.height, 500);
        assert_eq!(submitted.head_nonce, 77);
    }

    #[test]
    fn a_same_height_reorg_winner_is_not_submitted() {
        let _guard = mining_state_guard();
        // The node repacked height 600 on a different parent, so our solution can
        // no longer be rebuilt from its cached template.
        set_pending_block_stuff(600, pending_template_json(600, 0x22, 0xc1)).unwrap();

        let cnf = PoWorkConf::test_defaults("127.0.0.1:1".to_string(), 1, 16);
        let (res_tx, mut res_rx) = mpsc::sync_channel::<Arc<BlockMiningResult>>(4);
        let (submit_tx, submit_rx) = mpsc::sync_channel::<Arc<BlockMiningResult>>(4);

        let mut orphaned = BlockMiningResult::default();
        orphaned.height = 600;
        orphaned.prevhash = vec![0x11u8; HASH_WIDTH];
        orphaned.nonce_space = 1;
        orphaned.use_secs = 0.5;
        orphaned.head_nonce = 5;
        orphaned.coinbase_nonce = vec![0x07; HASH_WIDTH];
        orphaned.target_hash = vec![0x0f; HASH_WIDTH];
        orphaned.result_hash = vec![0x01; HASH_WIDTH];
        res_tx.send(Arc::new(orphaned)).unwrap();

        let mut most_hash = vec![255u8; HASH_WIDTH];
        let mut tracker = HashrateTracker::default();
        deal_block_mining_results(
            &cnf,
            &mut most_hash,
            &mut res_rx,
            1,
            &mut tracker,
            1,
            &submit_tx,
            &test_gate(),
        );
        assert!(submit_rx.try_recv().is_err());
    }

    #[test]
    fn malformed_pending_block_is_rejected_without_panicking() {
        assert!(set_pending_block_stuff(1, serde_json::json!({})).is_err());
        let invalid_hex = serde_json::json!({"target_hash": "zz"});
        assert!(set_pending_block_stuff(1, invalid_hex).is_err());
    }

    fn test_gate() -> SubmitGateHandle {
        Arc::new(Mutex::new(SubmitGate::default()))
    }

    /// A winner for one template identity (height, parent hash).
    fn winner_for_template(height: u64, prevhash: u8, head_nonce: u32) -> Arc<BlockMiningResult> {
        let mut res = BlockMiningResult::default();
        res.height = height;
        res.prevhash = vec![prevhash; HASH_WIDTH];
        res.head_nonce = head_nonce;
        res.nonce_space = 1;
        res.use_secs = 0.5;
        res.coinbase_nonce = vec![0x05; HASH_WIDTH];
        res.target_hash = vec![0x0f; HASH_WIDTH];
        res.result_hash = vec![0x01; HASH_WIDTH];
        Arc::new(res)
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

    #[test]
    fn only_the_first_winner_of_a_template_is_ever_submitted() {
        let _guard = mining_state_guard();
        // Reproduced on real hardware: at a low difficulty one GPU finds hundreds
        // of valid nonces per template, but a height holds exactly ONE block. Every
        // extra submit cost a blocking HTTP round trip, which filled the submit
        // queue, then the result channel, then blocked the workers and stopped the
        // GPU. Only the first one may leave; the rest are provably dead.
        set_pending_block_stuff(700, pending_template_json(700, 0x11, 0xd1)).unwrap();
        let cnf = PoWorkConf::test_defaults("127.0.0.1:1".to_string(), 1, 16);
        let (res_tx, mut res_rx) = mpsc::sync_channel::<Arc<BlockMiningResult>>(16);
        let (submit_tx, submit_rx) = mpsc::sync_channel::<Arc<BlockMiningResult>>(16);
        for head_nonce in 0..9u32 {
            res_tx
                .send(winner_for_template(700, 0x11, head_nonce))
                .unwrap();
        }

        let gate = test_gate();
        let mut most_hash = vec![255u8; HASH_WIDTH];
        let mut tracker = HashrateTracker::default();
        deal_block_mining_results(
            &cnf,
            &mut most_hash,
            &mut res_rx,
            8,
            &mut tracker,
            1,
            &submit_tx,
            &gate,
        );

        assert_eq!(
            submit_rx.try_recv().map(|w| w.head_nonce),
            Ok(0),
            "the first winner for a template is the payout and must always be submitted"
        );
        assert!(
            submit_rx.try_recv().is_err(),
            "a redundant winner for the same template can only come back as 'pending block height N not found'"
        );
        assert_eq!(
            lock_gate(&gate).templates[&(700u64, vec![0x11u8; HASH_WIDTH])].suppressed,
            8
        );
    }

    #[test]
    fn a_same_height_reorg_template_still_gets_its_own_submit() {
        let mut gate = SubmitGate::default();
        assert!(gate.admit(&winner_for_template(701, 0x11, 1)));
        assert!(!gate.admit(&winner_for_template(701, 0x11, 2)));
        // A different parent at the same height is a DIFFERENT template: the node
        // holds a different pending block for it, so this is a real chance at the
        // height and must never be gated by the first template.
        assert!(gate.admit(&winner_for_template(701, 0x22, 3)));
        assert!(!gate.admit(&winner_for_template(701, 0x22, 4)));
    }

    #[test]
    fn a_winner_at_a_new_height_is_never_suppressed() {
        let mut gate = SubmitGate::default();
        assert!(gate.admit(&winner_for_template(702, 0x11, 1)));
        gate.record_verdict(
            &winner_for_template(702, 0x11, 1),
            SubmitVerdict::Accepted,
            1,
        );
        assert!(!gate.admit(&winner_for_template(702, 0x11, 2)));
        assert!(gate.admit(&winner_for_template(703, 0x11, 1)));
    }

    #[test]
    fn a_template_the_upstream_keeps_serving_is_re_opened() {
        // A fullnode that took our block never serves that job again. An upstream
        // that hands us the SAME job after settling it has not consumed it (a pool
        // grading work at one height, or a block reorged back out), and our own
        // bookkeeping must not be what silences a live height.
        let mut gate = SubmitGate::default();
        let live = (704u64, vec![0x11u8; HASH_WIDTH]);
        let win = winner_for_template(704, 0x11, 1);
        assert!(gate.admit(&win));
        gate.record_verdict(&win, SubmitVerdict::Accepted, 7);
        assert!(!gate.admit(&winner_for_template(704, 0x11, 2)));

        // Same job, nothing re-served: it stays settled.
        gate.maintain(Some(&live), 704, 7);
        assert!(!gate.admit(&winner_for_template(704, 0x11, 3)));

        // Served again: one more winner gets its chance, and only one.
        gate.maintain(Some(&live), 704, 8);
        assert!(gate.admit(&winner_for_template(704, 0x11, 4)));
        assert!(!gate.admit(&winner_for_template(704, 0x11, 5)));
    }

    #[test]
    fn a_transport_failure_re_opens_the_template() {
        let mut gate = SubmitGate::default();
        let win = winner_for_template(800, 0x11, 1);
        assert!(gate.admit(&win));
        assert!(!gate.admit(&winner_for_template(800, 0x11, 2)));

        // No answer from the node proves nothing about the template.
        gate.record_verdict(&win, SubmitVerdict::NoNodeVerdict, 1);
        assert!(
            gate.admit(&winner_for_template(800, 0x11, 3)),
            "a network hiccup must never suppress a height for good"
        );
        // Neither does a rejection that is about one submission only.
        gate.record_verdict(&win, SubmitVerdict::SubmissionRejected, 1);
        assert!(gate.admit(&winner_for_template(800, 0x11, 4)));

        // A definitive verdict does settle it, and keeps it settled.
        gate.record_verdict(&win, SubmitVerdict::Accepted, 1);
        assert!(!gate.admit(&winner_for_template(800, 0x11, 5)));
        gate.record_verdict(&win, SubmitVerdict::NoNodeVerdict, 1);
        assert!(!gate.admit(&winner_for_template(800, 0x11, 6)));
    }

    #[test]
    fn the_submit_gate_stays_bounded_as_the_height_advances() {
        let mut gate = SubmitGate::default();
        for height in 1..=500u64 {
            let win = winner_for_template(height, 0x11, 1);
            gate.admit(&win);
            gate.record_verdict(&win, SubmitVerdict::Accepted, height);
            gate.maintain(Some(&(height, vec![0x11u8; HASH_WIDTH])), height, height);
            assert!(gate.templates.len() as u64 <= SUBMIT_GATE_HEIGHT_WINDOW + 1);
        }

        // A reorg storm at one standing height cannot grow it either.
        let mut gate = SubmitGate::default();
        for parent in 0..(SUBMIT_GATE_MAX_TEMPLATES as u32 * 3) {
            let mut res = BlockMiningResult::default();
            res.height = 900;
            res.prevhash = parent.to_be_bytes().to_vec();
            gate.admit(&res);
            gate.maintain(Some(&(900, Vec::new())), 900, parent as u64);
        }
        assert!(gate.templates.len() <= SUBMIT_GATE_MAX_TEMPLATES);
    }

    #[test]
    fn a_settled_template_is_reported_once_with_its_final_count() {
        let mut gate = SubmitGate::default();
        let key = (900u64, vec![0x11u8; HASH_WIDTH]);
        let win = winner_for_template(900, 0x11, 1);
        assert!(gate.admit(&win));
        for head_nonce in 2..12u32 {
            assert!(!gate.admit(&winner_for_template(900, 0x11, head_nonce)));
        }
        gate.record_verdict(&win, SubmitVerdict::Accepted, 3);

        // Still the live template, so the count is not final yet: no line.
        gate.maintain(Some(&key), 900, 3);
        assert!(!gate.templates[&key].reported);

        // The miner moved on: exactly one line, carrying the whole count.
        gate.maintain(Some(&(901, vec![0x33u8; HASH_WIDTH])), 901, 4);
        assert!(gate.templates[&key].reported);
        assert_eq!(gate.templates[&key].suppressed, 10);
        gate.maintain(Some(&(901, vec![0x33u8; HASH_WIDTH])), 901, 5);
        assert!(gate.templates[&key].reported);
    }

    #[test]
    fn only_a_node_verdict_about_the_template_settles_it() {
        let verdict = |body: &str| classify_submit_response(body).map(|(v, _)| v);
        assert_eq!(
            verdict("{\"ret\":0,\"height\":3,\"mining\":\"success\"}"),
            Some(SubmitVerdict::Accepted)
        );
        assert_eq!(
            verdict("{\"ret\":1,\"err\":\"pending block height 3 not found\"}"),
            Some(SubmitVerdict::TemplateGone)
        );
        assert_eq!(
            verdict("{\"ret\":1,\"err\":\"pending block not ready\"}"),
            Some(SubmitVerdict::TemplateGone)
        );
        assert_eq!(
            verdict(
                "{\"ret\":1,\"err\":\"difficulty check failed: expected at least 0f but got ff\"}"
            ),
            Some(SubmitVerdict::TemplateGone)
        );
        // The pool relay wraps an unrecognized upstream body as `msg`.
        assert_eq!(
            verdict("{\"ret\":1,\"msg\":\"pending block height 9 not found\"}"),
            Some(SubmitVerdict::TemplateGone)
        );
        // About this submission only: the template still deserves another try.
        assert_eq!(
            verdict("{\"ret\":1,\"err\":\"coinbase nonce format invalid\"}"),
            Some(SubmitVerdict::SubmissionRejected)
        );
        // No `ret` at all is a front-end hiccup, not a node decision.
        assert!(verdict("<html>502 Bad Gateway</html>").is_none());
        assert!(verdict("{\"msg\":\"gateway timeout\"}").is_none());

        assert!(SubmitVerdict::Accepted.ends_template());
        assert!(SubmitVerdict::TemplateGone.ends_template());
        assert!(!SubmitVerdict::SubmissionRejected.ends_template());
        assert!(!SubmitVerdict::NoNodeVerdict.ends_template());
    }

    #[test]
    fn both_upstream_dialects_are_classified() {
        // Measured against a real pool: 526 submits in 180 seconds, 519 of them
        // answered {"kind":"stale","ret":1} with NO error text at all. Reading only
        // the fullnode's error strings turned every one of those into "a rejection
        // about this one submission", which re-opened a template the pool had
        // already declared dead and paid for another round trip with the next
        // winner, and the next, and the next.
        let verdict = |body: &str| classify_submit_response(body).map(|(v, _)| v);

        // ---- pool dialect, exactly as its miner-API arm wraps it ----
        assert_eq!(
            verdict("{\"ret\":0,\"kind\":\"block\"}"),
            Some(SubmitVerdict::Accepted)
        );
        assert_eq!(
            verdict("{\"ret\":0,\"kind\":\"share\"}"),
            Some(SubmitVerdict::ShareCredited)
        );
        assert_eq!(
            verdict("{\"ret\":1,\"kind\":\"stale\"}"),
            Some(SubmitVerdict::TemplateGone)
        );
        assert_eq!(
            verdict("{\"ret\":1,\"kind\":\"busy\"}"),
            Some(SubmitVerdict::UpstreamBusy)
        );
        assert_eq!(
            verdict("{\"ret\":1,\"kind\":\"duplicate\"}"),
            Some(SubmitVerdict::DuplicateSubmission)
        );
        assert_eq!(
            verdict("{\"ret\":1,\"kind\":\"invalid\"}"),
            Some(SubmitVerdict::SubmissionRejected)
        );

        // ---- the pool's own object form, which keeps the extra fields ----
        assert_eq!(
            verdict("{\"ret\":0,\"kind\":\"block\",\"solved_height\":9}"),
            Some(SubmitVerdict::Accepted)
        );
        assert_eq!(
            verdict("{\"ret\":0,\"kind\":\"share\",\"accepted\":1234}"),
            Some(SubmitVerdict::ShareCredited)
        );
        assert_eq!(
            verdict("{\"ret\":1,\"kind\":\"stale\",\"height\":9}"),
            Some(SubmitVerdict::TemplateGone)
        );
        assert_eq!(
            verdict("{\"ret\":1,\"kind\":\"busy\",\"err\":\"too many shares this height\"}"),
            Some(SubmitVerdict::UpstreamBusy)
        );
        assert_eq!(
            verdict("{\"ret\":1,\"kind\":\"invalid\",\"err\":\"above share target\"}"),
            Some(SubmitVerdict::SubmissionRejected)
        );

        // ---- fullnode dialect, unchanged ----
        assert_eq!(
            verdict("{\"ret\":0,\"height\":3,\"mining\":\"success\"}"),
            Some(SubmitVerdict::Accepted)
        );
        assert_eq!(
            verdict("{\"ret\":1,\"err\":\"pending block height 3 not found\"}"),
            Some(SubmitVerdict::TemplateGone)
        );
        assert_eq!(
            verdict("{\"ret\":1,\"err\":\"pending block not ready\"}"),
            Some(SubmitVerdict::TemplateGone)
        );
        assert_eq!(
            verdict("{\"ret\":1,\"err\":\"difficulty check failed\"}"),
            Some(SubmitVerdict::TemplateGone)
        );
        assert_eq!(
            verdict("{\"ret\":1,\"err\":\"coinbase nonce format invalid\"}"),
            Some(SubmitVerdict::SubmissionRejected)
        );
        // An unmarked success keeps the meaning it always had here.
        assert_eq!(verdict("{\"ret\":0}"), Some(SubmitVerdict::Accepted));
        // A kind nobody here knows falls back to the same rules, so a future pool
        // word can never be read as a settle it did not mean.
        assert_eq!(
            verdict("{\"ret\":1,\"kind\":\"something-new\"}"),
            Some(SubmitVerdict::SubmissionRejected)
        );
        // No `ret` at all stays a transport-level non-verdict.
        assert!(verdict("<html>502 Bad Gateway</html>").is_none());
        assert!(verdict("{\"kind\":\"stale\"}").is_none());

        // The reason text still names what happened, for the submit log line.
        let (_, reason) = classify_submit_response("{\"ret\":1,\"kind\":\"stale\"}").unwrap();
        assert!(reason.contains("stale"), "the log line must name the kind");

        // ---- what each verdict does to the template ----
        assert!(SubmitVerdict::Accepted.ends_template());
        assert!(SubmitVerdict::TemplateGone.ends_template());
        assert!(!SubmitVerdict::ShareCredited.ends_template());
        assert!(!SubmitVerdict::UpstreamBusy.ends_template());
        assert!(!SubmitVerdict::DuplicateSubmission.ends_template());
        assert!(!SubmitVerdict::SubmissionRejected.ends_template());
        assert!(!SubmitVerdict::NoNodeVerdict.ends_template());
        assert!(SubmitVerdict::ShareCredited.credits_share());
        assert!(!SubmitVerdict::Accepted.credits_share());
        assert_eq!(
            SubmitVerdict::UpstreamBusy.retry_cooldown(),
            Some(POOL_BUSY_COOLDOWN)
        );
        assert_eq!(SubmitVerdict::ShareCredited.retry_cooldown(), None);
        assert_eq!(SubmitVerdict::SubmissionRejected.retry_cooldown(), None);

        // A template the pool called stale must not be reported as a rejected
        // submission: the operator line is the only thing they see.
        let stale_line = SubmitVerdict::TemplateGone.outcome_text();
        assert!(stale_line.contains("stale"));
        assert!(!stale_line.contains("rejection"));
    }

    #[test]
    fn every_pool_share_for_one_template_is_submitted_and_paid() {
        // THE money guarantee. On mainnet the pool's share target is 2^24 easier
        // than the network target, so "share" is the ordinary answer and "block"
        // is rare. Each share is credited work, so treating the first one as a
        // settle would drop every later share for that template on the floor:
        // systematic underpayment of the miner, invisible in the logs.
        let mut gate = SubmitGate::default();
        let key = (950u64, vec![0x11u8; HASH_WIDTH]);
        let (share, _) = classify_submit_response("{\"ret\":0,\"kind\":\"share\"}").unwrap();
        assert_eq!(share, SubmitVerdict::ShareCredited);

        // The pool answers every single submission with a credited share.
        let mut submitted = 0u32;
        for head_nonce in 0..200u32 {
            let win = winner_for_template(950, 0x11, head_nonce);
            if gate.admit(&win) {
                submitted += 1;
                gate.record_verdict(&win, share, 1);
            }
        }
        assert_eq!(
            submitted, 200,
            "every share a pool credits is money: not one may be dropped locally"
        );
        assert_eq!(
            gate.templates[&key].suppressed, 0,
            "a credited share is never a redundant winner"
        );
        assert_ne!(
            gate.templates[&key].state,
            TemplateSubmitState::Settled,
            "a share credit must never settle the template"
        );

        // The real drain admits a whole batch before any verdict comes back. Once
        // the upstream has proved it pays per share, none of that batch may be
        // dropped either.
        let mut batch = 0u32;
        for head_nonce in 200..260u32 {
            if gate.admit(&winner_for_template(950, 0x11, head_nonce)) {
                batch += 1;
            }
        }
        assert_eq!(
            batch, 60,
            "a burst of shares must not be gated by the first"
        );
        assert_eq!(gate.templates[&key].suppressed, 0);

        // The NEXT template inherits it. Otherwise every job change would start
        // out "solo" again and eat the burst of shares found before its first
        // verdict came back, which is earned income, once per template.
        let mut fresh = 0u32;
        for head_nonce in 0..40u32 {
            if gate.admit(&winner_for_template(951, 0x22, head_nonce)) {
                fresh += 1;
            }
        }
        assert_eq!(
            fresh, 40,
            "a pool that pays per share keeps paying at the next height"
        );
        assert_eq!(
            gate.templates[&(951u64, vec![0x22u8; HASH_WIDTH])].suppressed,
            0
        );
    }

    #[test]
    fn a_share_paying_template_submits_every_winner_of_a_drain() {
        let _guard = mining_state_guard();
        // Same guarantee, through the real drain path: with a fullnode only the
        // first winner of a template leaves, with a share-paying pool all of them do.
        set_pending_block_stuff(710, pending_template_json(710, 0x11, 0xd1)).unwrap();
        let cnf = PoWorkConf::test_defaults("127.0.0.1:1".to_string(), 1, 16);
        let (res_tx, mut res_rx) = mpsc::sync_channel::<Arc<BlockMiningResult>>(16);
        let (submit_tx, submit_rx) = mpsc::sync_channel::<Arc<BlockMiningResult>>(16);

        // One earlier share told us this upstream credits them.
        let gate = test_gate();
        let primer = winner_for_template(710, 0x11, 99);
        assert!(lock_gate(&gate).admit(&primer));
        lock_gate(&gate).record_verdict(&primer, SubmitVerdict::ShareCredited, 1);

        for head_nonce in 0..9u32 {
            res_tx
                .send(winner_for_template(710, 0x11, head_nonce))
                .unwrap();
        }
        let mut most_hash = vec![255u8; HASH_WIDTH];
        let mut tracker = HashrateTracker::default();
        deal_block_mining_results(
            &cnf,
            &mut most_hash,
            &mut res_rx,
            8,
            &mut tracker,
            1,
            &submit_tx,
            &gate,
        );

        let mut queued = Vec::new();
        while let Ok(win) = submit_rx.try_recv() {
            queued.push(win.head_nonce);
        }
        assert_eq!(
            queued,
            (0..9u32).collect::<Vec<u32>>(),
            "every winner of a share-paying template is separately payable work"
        );
        assert_eq!(
            lock_gate(&gate).templates[&(710u64, vec![0x11u8; HASH_WIDTH])].suppressed,
            0
        );
    }

    #[test]
    fn a_pool_stale_reply_settles_the_template() {
        // The 519 wasted round trips: "stale" carries no error text, so it used to
        // read as a rejection of one submission and re-opened the template.
        let mut gate = SubmitGate::default();
        let key = (960u64, vec![0x11u8; HASH_WIDTH]);
        let win = winner_for_template(960, 0x11, 1);
        let (stale, _) = classify_submit_response("{\"ret\":1,\"kind\":\"stale\"}").unwrap();
        assert!(gate.admit(&win));
        gate.record_verdict(&win, stale, 1);
        assert!(
            !gate.admit(&winner_for_template(960, 0x11, 2)),
            "a template the pool called stale can never take another winner"
        );
        assert_eq!(gate.templates[&key].state, TemplateSubmitState::Settled);
        assert_eq!(
            gate.templates[&key].outcome,
            SubmitVerdict::TemplateGone.outcome_text()
        );

        // A duplicate is about ONE submission (the same height, coinbase nonce and
        // block nonce), so a DIFFERENT nonce is still wanted: it must not settle.
        let mut gate = SubmitGate::default();
        let win = winner_for_template(961, 0x11, 1);
        let (dup, _) = classify_submit_response("{\"ret\":1,\"kind\":\"duplicate\"}").unwrap();
        assert!(gate.admit(&win));
        gate.record_verdict(&win, dup, 1);
        assert!(gate.admit(&winner_for_template(961, 0x11, 2)));
        assert_ne!(
            gate.templates[&(961u64, vec![0x11u8; HASH_WIDTH])].state,
            TemplateSubmitState::Settled
        );
    }

    #[test]
    fn a_busy_pool_is_backed_off_without_killing_its_template() {
        let mut gate = SubmitGate::default();
        let key = (970u64, vec![0x11u8; HASH_WIDTH]);
        let win = winner_for_template(970, 0x11, 1);
        let (busy, _) = classify_submit_response("{\"ret\":1,\"kind\":\"busy\"}").unwrap();
        let t0 = Instant::now();
        assert!(gate.admit_at(&win, t0));
        gate.record_verdict_at(&win, busy, 1, t0);
        assert_ne!(
            gate.templates[&key].state,
            TemplateSubmitState::Settled,
            "an overloaded pool has not consumed the template"
        );
        assert!(
            !gate.admit_at(
                &winner_for_template(970, 0x11, 2),
                t0 + Duration::from_millis(123)
            ),
            "a pool that just said it is overloaded must not be hammered"
        );
        assert!(
            gate.admit_at(&winner_for_template(970, 0x11, 3), t0 + POOL_BUSY_COOLDOWN),
            "the pause is a back-off, not a settle: the template is still alive"
        );

        // Any other answer clears the pause at once, so a recovered pool is used
        // again immediately.
        gate.record_verdict_at(
            &win,
            SubmitVerdict::ShareCredited,
            1,
            t0 + POOL_BUSY_COOLDOWN,
        );
        assert!(gate.admit_at(&winner_for_template(970, 0x11, 4), t0 + POOL_BUSY_COOLDOWN));
    }

    #[test]
    fn the_fullnode_gate_behaviour_is_unchanged() {
        // Solo mining must behave exactly as it did: a height holds one block, so
        // an accepted one settles the template, and a transport non-verdict never
        // silences a height.
        let (accepted, _) =
            classify_submit_response("{\"ret\":0,\"height\":3,\"mining\":\"success\"}").unwrap();
        assert_eq!(accepted, SubmitVerdict::Accepted);
        let mut gate = SubmitGate::default();
        let win = winner_for_template(980, 0x11, 1);
        assert!(gate.admit(&win));
        gate.record_verdict(&win, accepted, 1);
        assert!(
            !gate.admit(&winner_for_template(980, 0x11, 2)),
            "an accepted block still settles the height"
        );
        assert_eq!(
            gate.templates[&(980u64, vec![0x11u8; HASH_WIDTH])].suppressed,
            1
        );

        assert!(classify_submit_response("<html>502 Bad Gateway</html>").is_none());
        let mut gate = SubmitGate::default();
        let win = winner_for_template(981, 0x11, 1);
        assert!(gate.admit(&win));
        gate.record_verdict(&win, SubmitVerdict::NoNodeVerdict, 1);
        assert!(
            gate.admit(&winner_for_template(981, 0x11, 2)),
            "a network hiccup must never permanently silence a height"
        );
    }
}
