//! Hacash pool server: serves work, validates shares, keeps PPLNS accounting,
//! submits full blocks, and settles payouts. Blocking HTTP on std::net — no
//! async runtime, no node changes.
//!
//! Speaks the STANDARD miner API (so an unmodified poworker can mine here) with
//! one difference: `target_hash` carries the pool's SHARE target. A submission
//! is promoted to a real block whenever it also beats the network target.
//!
//! Protections that matter once other people's hashrate is involved:
//!   * duplicate shares are rejected (a resubmitted solution cannot inflate a
//!     miner's PPLNS credit at everyone else's expense)
//!   * accounting is persisted atomically, so a restart never erases work
//!   * a submitted block only counts once the chain still holds OUR hash at that
//!     height — orphans are detected and not paid for
//!   * settlement runs automatically on a timer, is idempotent across restarts,
//!     and chunks into <=200-action transactions the node will accept
//!   * one panicking request or poisoned lock cannot freeze or crash the pool
//!   * per-IP connection caps + a separate long-poll budget stop one host from
//!     exhausting every connection slot
//!
//! Endpoints: /work, /share, /stats (own protocol) and /query/miner/pending,
//! /query/miner/notice, /submit/miner/success (standard API).
//!
//! Usage: pool-server <node> <wallet_file> <listen> <share_bits> <chain> [settle_secs]
//!   `chain` is REQUIRED (mainnet|testnet) — a wrong difficulty rule makes every
//!   share/block the node rejects, so there is no silent default.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use basis::interface::*;
use field::*;
use protocol::action::HacToTrs;
use protocol::transaction::TransactionType2;
use sys::curtimes;

use pool_spike::difficulty::ChainParams;
use pool_spike::pool_core::{self, Pplns, split_payout};
use pool_spike::{
    Template, assemble_block, balance, balance_units, coinbase_body_hex, coinbase_with_extranonce,
    fetch_template, find_str, find_u64, get_json, http_client, intro_bytes, is_payout_address,
    load_or_create_wallet, post_hex, submit_block_bytes,
};

use serde_json::json;

const PPLNS_WINDOW: usize = 4096;

/// Global live-connection cap and the per-source-IP cap. The per-IP cap is what
/// actually stops one host pinning every slot with long-polls; the global cap is
/// a coarse backstop.
const MAX_CONNS: usize = 1024;
const MAX_PER_IP: u32 = 24;
/// Long-poll waiters are budgeted separately so they can never consume the whole
/// connection pool and starve short /work, /share and /submit requests.
const MAX_NOTICE_WAITERS: usize = 384;
/// Per-height replay-protection set bound: beyond this a template is producing an
/// implausible flood, so reject further shares this height rather than grow memory
/// without limit. Reset every time the height advances.
const SEEN_CAP: usize = 2_000_000;
/// Recipients per settlement transaction. The node enforces TX_ACTIONS_MAX = 200
/// actions; stay safely under it so a large payout is chunked, never rejected.
const PAYOUT_CHUNK: usize = 190;

static CONNS: AtomicUsize = AtomicUsize::new(0);
static NOTICE_WAITERS: AtomicUsize = AtomicUsize::new(0);
static PER_IP: LazyLock<Mutex<HashMap<IpAddr, u32>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Lock the pool, recovering from a poisoned mutex instead of cascading panics.
/// With panic=unwind a handler that panics under the lock poisons it; recovering
/// keeps the pool serving instead of turning one fault into permanent death.
fn plock(m: &Mutex<Pool>) -> MutexGuard<'_, Pool> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn per_ip_lock() -> MutexGuard<'static, HashMap<IpAddr, u32>> {
    PER_IP.lock().unwrap_or_else(|e| e.into_inner())
}

/// Releases a connection's global + per-IP slot on scope exit — including on an
/// unwind — so a panicking handler can never leak a slot and wedge the listener.
struct ConnGuard {
    ip: Option<IpAddr>,
}
impl Drop for ConnGuard {
    fn drop(&mut self) {
        CONNS.fetch_sub(1, Relaxed);
        if let Some(ip) = self.ip {
            let mut m = per_ip_lock();
            if let Some(c) = m.get_mut(&ip) {
                *c -= 1;
                if *c == 0 {
                    m.remove(&ip);
                }
            }
        }
    }
}

/// Releases a long-poll waiter slot on scope exit.
struct NoticeGuard;
impl Drop for NoticeGuard {
    fn drop(&mut self) {
        NOTICE_WAITERS.fetch_sub(1, Relaxed);
    }
}

/// Atomic file write (temp + optional fsync + rename) so a crash or full disk
/// mid-write can never leave a truncated/corrupt file. `durable` fsyncs before
/// rename; the frequent debounced share-save skips it (a crash loses at most the
/// last handful of shares, which is already the accepted tolerance) so the pool
/// lock is not held across an fsync stall.
fn atomic_write(path: &str, body: &[u8], durable: bool) -> std::io::Result<()> {
    let tmp = format!("{path}.tmp.{}", std::process::id());
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body)?;
        if durable {
            let _ = f.sync_all();
        }
    }
    std::fs::rename(&tmp, path)
}

struct Pool {
    node: String,
    payout: String,
    state_file: String,
    client: reqwest::blocking::Client,
    params: ChainParams,
    tpl: Template,
    share_target: [u8; 32],
    /// How many powers of two EASIER than the network target a share is. The
    /// share target is derived from the live network difficulty (not an absolute
    /// value), so it scales as difficulty changes and a share represents a fixed
    /// fraction of a real block — which is what makes credit proportional to
    /// hashrate instead of to batch cadence.
    share_factor: u32,
    network_target: [u8; 32],
    /// Cached /query/miner/pending response for the current template, rebuilt
    /// only when the template changes so a poll never rebuilds it under the lock.
    pending_cache: String,
    workers: HashMap<String, [u8; 32]>,
    next_en: u64,
    pplns: Pplns,
    accepted: u64,
    blocks: u64,
    orphaned: u64,
    /// Solutions already credited for the current template — rejects replays.
    seen: HashSet<(u64, [u8; 32], u32)>,
    /// Blocks we submitted, awaiting confirmation that they stuck.
    submitted: Vec<(u64, [u8; 32])>,
    /// Accepted shares not yet flushed to disk (debounces state writes).
    unsaved: u32,
    /// Hashes of payout transactions that have not yet confirmed. While ANY is
    /// still in the mempool we must not settle again (double spend). Persisted so
    /// a restart mid-settlement does not re-pay. Robust to a lost submit ACK and
    /// to the wallet also earning coinbase income.
    settle_pending_txs: Vec<String>,
}

impl Pool {
    /// Flush accounting to disk at most every 16 shares. Block events call
    /// save_state directly, so a crash loses at worst a handful of shares.
    fn note_share_saved(&mut self) {
        self.unsaved += 1;
        if self.unsaved >= 16 {
            // Non-durable: high frequency, and losing <=16 shares on a crash is
            // already the accepted tolerance. Block/settle events fsync.
            self.persist(false);
            self.unsaved = 0;
        }
    }

    /// Derive the share target from the CURRENT network difficulty, so it tracks
    /// difficulty changes instead of being a fixed absolute threshold.
    fn recompute_share_target(&mut self) {
        self.share_target = pool_core::share_target_hash(self.tpl.difficulty, self.share_factor);
    }

    /// Rebuild the cached standard-API pending response for the current template.
    fn rebuild_pending_cache(&mut self) {
        let cb = coinbase_with_extranonce(&self.tpl, &[0u8; 32]);
        let intro = intro_bytes(&self.tpl, &cb, 0);
        self.pending_cache = json!({
            "ret": 0,
            "height": self.tpl.height,
            "block_intro": hex::encode(intro),
            "target_hash": hex::encode(self.share_target),
            "coinbase_body": coinbase_body_hex(&cb),
            "mkrl_modify_list": [],
        })
        .to_string();
    }

    /// Stable per-worker extranonce -> private search space (coinbase miner_nonce).
    /// The /work protocol is anonymous, so cap the map: past the cap, hand out a
    /// deterministic extranonce derived from the name instead of storing it, so a
    /// flood of unique names cannot grow memory without bound.
    fn extranonce_for(&mut self, worker: &str) -> [u8; 32] {
        if let Some(en) = self.workers.get(worker) {
            return *en;
        }
        if self.workers.len() >= 100_000 {
            let mut en = [0u8; 32];
            en[0..8].copy_from_slice(&(worker.len() as u64).to_be_bytes());
            for (i, b) in worker.bytes().enumerate() {
                en[8 + (i % 24)] ^= b;
            }
            return en;
        }
        self.next_en += 1;
        let mut en = [0u8; 32];
        en[24..32].copy_from_slice(&self.next_en.to_be_bytes());
        self.workers.insert(worker.to_string(), en);
        en
    }

    /// Durable state flush (fsync). Used for block-found and settlement events.
    fn save_state(&self) {
        self.persist(true);
    }

    fn persist(&self, durable: bool) {
        if self.state_file.is_empty() {
            return;
        }
        let body = json!({
            "window": PPLNS_WINDOW,
            "order": self.pplns.snapshot(),
            "accepted": self.accepted,
            "blocks": self.blocks,
            "orphaned": self.orphaned,
            "settle_pending_txs": self.settle_pending_txs,
        });
        if let Err(e) = atomic_write(&self.state_file, body.to_string().as_bytes(), durable) {
            eprintln!("[state] save failed ({e}); accounting NOT flushed this round");
        }
    }

    fn load_state(&mut self) {
        let Ok(txt) = std::fs::read_to_string(&self.state_file) else {
            return;
        };
        let j: serde_json::Value = match serde_json::from_str(&txt) {
            Ok(j) => j,
            Err(e) => {
                // Never silently wipe accounting: preserve the corrupt file and
                // start fresh only after loudly flagging it for the operator.
                let bak = format!("{}.corrupt.{}", self.state_file, std::process::id());
                let _ = std::fs::rename(&self.state_file, &bak);
                eprintln!(
                    "[state] file corrupt ({e}); preserved as {bak}, starting with empty accounting"
                );
                return;
            }
        };
        let order: Vec<String> = j
            .get("order")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        self.pplns = Pplns::restore(PPLNS_WINDOW, order);
        self.accepted = j.get("accepted").and_then(|v| v.as_u64()).unwrap_or(0);
        self.blocks = j.get("blocks").and_then(|v| v.as_u64()).unwrap_or(0);
        self.orphaned = j.get("orphaned").and_then(|v| v.as_u64()).unwrap_or(0);
        self.settle_pending_txs = j
            .get("settle_pending_txs")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        println!(
            "restored accounting: {} shares in window, {} blocks, {} orphaned, {} payout(s) pending",
            self.pplns.total(),
            self.blocks,
            self.orphaned,
            self.settle_pending_txs.len()
        );
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let node = a
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:8088".to_string());
    let node = node.trim_end_matches('/').to_string();
    let wallet_file = a
        .get(2)
        .cloned()
        .unwrap_or_else(|| "pool-wallet.key".to_string());
    let listen = a
        .get(3)
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:9777".to_string());
    // How many powers of two easier than a network block a share is. Tune to the
    // miner population and GPU batch size: too small and small miners rarely find
    // a share; too large and a whole GPU batch's best hash always beats it, so
    // credit tracks batch cadence rather than hashrate. ~24 suits GPU batches.
    let share_factor: u32 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(24);
    // chain is REQUIRED: a mainnet pool run with testnet difficulty (or vice
    // versa) computes the wrong target and every block/share is rejected. Refuse
    // to guess.
    let Some(chain) = a.get(5).cloned() else {
        eprintln!(
            "usage: pool-server <node> <wallet_file> <listen> <share_bits> <chain> [settle_secs]\n\
             `chain` is required and must be `mainnet` or `testnet`."
        );
        std::process::exit(2);
    };
    if chain != "mainnet" && chain != "testnet" {
        eprintln!("chain must be `mainnet` or `testnet` (got `{chain}`)");
        std::process::exit(2);
    }
    let settle_secs: u64 = a.get(6).and_then(|s| s.parse().ok()).unwrap_or(300);
    let params = ChainParams::from_name(&chain);

    println!("== pool-server ==");
    println!("node    = {node}");
    let wallet = load_or_create_wallet(&wallet_file);
    let payout = wallet.readable().to_string();

    let client = http_client();
    let tpl = fetch_template(&client, &node, &payout, &params)
        .expect("could not fetch an initial template — is the node running and synced?");
    let network_target = tpl.target;

    println!("listen  = {listen}");
    println!("chain   = {chain} (ASERT at height {})", params.asert_height);
    println!("share   = 2^{share_factor} easier than a network block");
    println!("settle  = every {settle_secs}s");
    println!(
        "height  = {} (template, difficulty {})",
        tpl.height, tpl.difficulty
    );

    let mut pool = Pool {
        node: node.clone(),
        payout,
        state_file: format!("{wallet_file}.state.json"),
        client,
        params,
        share_target: pool_core::share_target_hash(tpl.difficulty, share_factor),
        tpl,
        share_factor,
        network_target,
        pending_cache: String::new(),
        workers: HashMap::new(),
        next_en: 0,
        pplns: Pplns::new(PPLNS_WINDOW),
        accepted: 0,
        blocks: 0,
        orphaned: 0,
        seen: HashSet::new(),
        submitted: Vec::new(),
        unsaved: 0,
        settle_pending_txs: Vec::new(),
    };
    pool.load_state();
    pool.rebuild_pending_cache();
    let pool = Arc::new(Mutex::new(pool));

    // Background: keep the template current with the chain tip and confirm our
    // submitted blocks. All node HTTP happens OFF the pool lock, so miners are
    // never stalled by it. This also advances work when the NETWORK finds a
    // block, not only when we do. The whole loop body is panic-isolated so a
    // single transient fault can never kill this long-lived thread.
    {
        let pool = pool.clone();
        let (client, node, payout, params) = {
            let p = plock(&pool);
            (
                p.client.clone(),
                p.node.clone(),
                p.payout.clone(),
                p.params.clone(),
            )
        };
        std::thread::spawn(move || {
            loop {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    template_cycle(&pool, &client, &node, &payout, &params);
                }));
                if let Err(e) = r {
                    eprintln!("[template] cycle panicked, continuing: {e:?}");
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
    }

    // Automatic settlement on a timer.
    {
        let p = pool.clone();
        let wf = wallet_file.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(settle_secs));
                // One bad settle cycle (poisoned lock, wallet issue, future
                // refactor) must never permanently kill payouts.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    settle_once(&p, &wf)
                }));
                if let Err(e) = r {
                    eprintln!("[settle] cycle panicked, continuing: {e:?}");
                }
            }
        });
    }

    let listener = TcpListener::bind(&listen).expect("bind");
    println!("listening...\n");
    for stream in listener.incoming() {
        let s = match stream {
            Ok(s) => s,
            // A single accept() error (e.g. EMFILE under load) must not tear down
            // the whole listener — log and keep serving.
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };
        if CONNS.load(Relaxed) >= MAX_CONNS {
            continue; // drop: s closes as it goes out of scope
        }
        let ip = s.peer_addr().ok().map(|a| a.ip());
        // Per-IP admission: one source cannot hold more than MAX_PER_IP slots,
        // so a single host cannot pin every connection with long-polls.
        if let Some(ip) = ip {
            let mut m = per_ip_lock();
            let c = m.entry(ip).or_insert(0);
            if *c >= MAX_PER_IP {
                continue; // drop this connection from a noisy IP
            }
            *c += 1;
        }
        CONNS.fetch_add(1, Relaxed);
        let guard = ConnGuard { ip };
        let p = pool.clone();
        std::thread::spawn(move || {
            let _g = guard; // releases the slot on return AND on unwind
            handle(s, p);
        });
    }
}

/// One iteration of the background template/confirmation loop (panic-isolated by
/// the caller). Refreshes the template on a height change and confirms/orphans
/// our submitted blocks. All node HTTP is done before taking the pool lock.
fn template_cycle(
    pool: &Arc<Mutex<Pool>>,
    client: &reqwest::blocking::Client,
    node: &str,
    payout: &str,
    params: &ChainParams,
) {
    let fresh = fetch_template(client, node, payout, params);
    let pending: Vec<(u64, [u8; 32])> = plock(pool).submitted.clone();
    let tip = fresh.as_ref().map(|t| t.height.saturating_sub(1));
    let mut confirmed = Vec::new();
    let mut orphaned = Vec::new();
    for (h, ours) in &pending {
        if tip.map(|t| *h > t).unwrap_or(true) {
            continue; // not buried yet, or no tip this cycle
        }
        let j = get_json(client, &format!("{node}/query/block/intro?height={h}"));
        match find_str(&j, "hash") {
            Some(chain_hash) if chain_hash == hex::encode(ours) => confirmed.push((*h, *ours)),
            Some(chain_hash) => {
                orphaned.push((*h, *ours));
                println!("[reorg] our block {h} orphaned (chain holds {chain_hash})");
            }
            None => {} // node has not stored it yet; keep waiting
        }
    }
    {
        let mut p = plock(pool);
        if let Some(t) = fresh {
            // Replace the template when the tip changes: either a new height, or
            // a same-height reorg (different prev-hash). At the same height and
            // same prev-hash the timestamp/difficulty are fixed, so keeping the
            // template valid keeps every worker's in-flight share valid.
            let changed = t.height != p.tpl.height || t.prevhash != p.tpl.prevhash;
            if changed {
                p.tpl = t;
                p.network_target = p.tpl.target;
                // Re-derive the share target from the new difficulty so shares stay
                // a fixed fraction of a block as difficulty moves.
                p.recompute_share_target();
                p.seen.clear();
                p.rebuild_pending_cache();
            }
        }
        p.blocks += confirmed.len() as u64;
        p.orphaned += orphaned.len() as u64;
        p.submitted
            .retain(|e| !confirmed.contains(e) && !orphaned.contains(e));
    }
}

/// Pay every miner their PPLNS share of the pool's spendable balance. Splits the
/// distributable balance over PAYABLE workers only, then submits one or more
/// transactions (chunked to <=PAYOUT_CHUNK actions each) so a large payout is
/// never rejected by the node's 200-action limit. Idempotent across restarts via
/// the persisted `settle_pending_txs`.
fn settle_once(pool: &Arc<Mutex<Pool>>, wallet_file: &str) {
    let (node, counts, pending_txs) = {
        let p = plock(pool);
        (
            p.node.clone(),
            p.pplns.counts(),
            p.settle_pending_txs.clone(),
        )
    };
    if counts.is_empty() {
        return;
    }
    let client = http_client();

    // Resolve any outstanding payouts FIRST, using the node's own view of each tx
    // rather than the wallet balance. Correct even if a submit ACK was lost and
    // even though the same wallet keeps earning coinbase income.
    if !pending_txs.is_empty() {
        let mut still = Vec::new();
        for hx in &pending_txs {
            let j = get_json(&client, &format!("{node}/query/transaction?hash={hx}"));
            let ret_ok = find_u64(&j, "ret") == Some(0);
            let is_pending = j
                .get("data")
                .and_then(|d| d.get("pending"))
                .and_then(|v| v.as_bool())
                .or_else(|| j.get("pending").and_then(|v| v.as_bool()))
                .unwrap_or(false);
            if ret_ok && is_pending {
                still.push(hx.clone());
            }
        }
        if !still.is_empty() {
            // Some payout is still in the mempool; keep only those and skip.
            let mut p = plock(pool);
            p.settle_pending_txs = still;
            p.save_state();
            return;
        }
        // All resolved (confirmed or gone): clear and settle fresh income.
        let mut p = plock(pool);
        p.settle_pending_txs.clear();
        p.save_state();
    }

    let acc = load_or_create_wallet(wallet_file);
    let bal = balance(&client, &node, acc.readable());
    let units = balance_units(&bal);

    // Keep a reserve so the wallet always covers the (per-chunk) tx fee. No pool
    // fee is skimmed: this is a community pool, and the reserve covers the fees.
    let reserve = 5u64; // 0.5 HAC — covers up to ~50 chunk fees of 0.01 HAC each
    if units <= reserve + 1 {
        return;
    }
    let distributable = units - reserve;

    // Split over PAYABLE workers only, so IP-fallback / unpayable keys do not
    // dilute the honest miners' proportional share.
    let payable_counts: Vec<(String, u64)> = counts
        .into_iter()
        .filter(|(w, _)| is_payout_address(w))
        .collect();
    if payable_counts.is_empty() {
        return;
    }
    let split = split_payout(distributable, 0, 1, &payable_counts);
    if split.is_empty() {
        return;
    }

    let main = Address::from(*acc.address());
    let mut total_paid = 0u64;
    let mut recipients = 0usize;
    for chunk in split.chunks(PAYOUT_CHUNK) {
        let fee = match Amount::from("1:246") {
            Ok(f) => f, // 0.01 HAC tx fee (from reserve)
            Err(_) => {
                eprintln!("[settle] internal: bad fee literal");
                return;
            }
        };
        let mut tx = TransactionType2::new_by(main.clone(), fee, curtimes());
        let mut pushed = 0usize;
        let mut chunk_units = 0u64;
        for (addr, u) in chunk {
            let Ok(to) = Address::from_readable(addr) else {
                continue;
            };
            let Ok(amt) = Amount::from(&format!("{u}:247")) else {
                eprintln!("[settle] skip {addr}: amount {u}:247 not representable");
                continue;
            };
            let mut act = HacToTrs::new();
            act.to = AddrOrPtr::from_addr(to);
            act.hacash = amt;
            if tx.push_action(Box::new(act)).is_err() {
                break; // should not happen within a <=190 chunk, but stay safe
            }
            pushed += 1;
            chunk_units += *u;
        }
        if pushed == 0 {
            continue;
        }
        if tx.fill_sign(&acc).is_err() {
            eprintln!("[settle] signing failed for a chunk; skipping it");
            continue;
        }
        // Record the payout tx hash BEFORE submitting AND persist it, so a lost
        // ACK or a crash mid-settlement still blocks a second payout: next cycle
        // we poll this hash and only retry if it is gone.
        let txhash = hex::encode(tx.hash().serialize());
        {
            let mut p = plock(pool);
            p.settle_pending_txs.push(txhash.clone());
            p.save_state();
        }
        let body = hex::encode(tx.serialize());
        let resp = post_hex(
            &client,
            &format!("{node}/submit/transaction?hexbody=true"),
            &body,
        );
        // Surface a node rejection instead of silently reporting success.
        let accepted = serde_json::from_str::<serde_json::Value>(&resp)
            .ok()
            .and_then(|v| find_u64(&v, "ret"))
            == Some(0);
        if !accepted {
            eprintln!(
                "[settle] node did NOT accept payout tx {} ({} recipients): {resp}",
                &txhash[..txhash.len().min(16)],
                pushed
            );
        } else {
            println!(
                "[settle] submitted payout tx {} paying {pushed} miner(s) {chunk_units} units",
                &txhash[..txhash.len().min(16)]
            );
        }
        total_paid += chunk_units;
        recipients += pushed;
    }
    println!(
        "[settle] settlement done: {recipients} recipient(s), {total_paid} units across {} tx(s)",
        split.chunks(PAYOUT_CHUNK).len()
    );
}

/// Wraps the share-credit path: takes the pool lock only twice (a brief snapshot
/// and a brief commit), computing the expensive x16rs hash OFF the lock so one
/// submission cannot serialize every other miner behind a full PoW evaluation.
/// Replay protection stays atomic: the (height, coinbase_nonce, block_nonce) key
/// is inserted under the commit lock and a second submission of the same key is
/// rejected there.
fn handle_submission(
    pool: &Arc<Mutex<Pool>>,
    worker: &str,
    height: u64,
    coinbase_nonce: [u8; 32],
    block_nonce: u32,
) -> serde_json::Value {
    let key = (height, coinbase_nonce, block_nonce);
    // Phase 1 — brief lock: reject stale/duplicate early and snapshot the inputs.
    let (tpl, share_target, network_target, client, node) = {
        let p = plock(pool);
        if height != p.tpl.height {
            return json!({"ok":false,"kind":"stale","height":p.tpl.height});
        }
        if p.seen.contains(&key) {
            return json!({"ok":false,"kind":"duplicate"});
        }
        (
            p.tpl.clone(),
            p.share_target,
            p.network_target,
            p.client.clone(),
            p.node.clone(),
        )
    };

    // Phase 2 — no lock: rebuild exactly what the worker hashed and evaluate the
    // (deliberately slow) x16rs PoW hash without blocking any other request.
    let cb = coinbase_with_extranonce(&tpl, &coinbase_nonce);
    let intro = intro_bytes(&tpl, &cb, block_nonce);
    let hash = pool_core::hash_of(tpl.height, &intro);
    if !pool_core::beats(&hash, &share_target) {
        return json!({"ok":false,"kind":"invalid","err":"above share target"});
    }
    let is_block = pool_core::beats(&hash, &network_target);

    // Phase 3 — brief lock: atomically re-check freshness + replay, then credit.
    let block_bytes = {
        let mut p = plock(pool);
        if height != p.tpl.height {
            return json!({"ok":false,"kind":"stale","height":p.tpl.height});
        }
        if p.seen.len() >= SEEN_CAP {
            return json!({"ok":false,"kind":"busy","err":"too many shares this height"});
        }
        if !p.seen.insert(key) {
            return json!({"ok":false,"kind":"duplicate"});
        }
        p.pplns.record(worker);
        p.accepted += 1;
        if !is_block {
            p.note_share_saved();
            return json!({"ok":true,"kind":"share","accepted":p.accepted});
        }
        let bytes = assemble_block(&tpl, &cb, block_nonce);
        let solved = tpl.height;
        p.submitted.push((solved, hash)); // counted once the bg thread sees it stick
        p.save_state();
        bytes
    };

    // Phase 4 — no lock: submit the winning block.
    let submit = submit_block_bytes(&client, &node, &block_bytes);
    json!({"ok":true,"kind":"block","solved_height":height,"submit":submit})
}

fn parse32(s: Option<&String>) -> Option<[u8; 32]> {
    let v = hex::decode(s?).ok()?;
    if v.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Some(out)
}

fn handle(mut s: TcpStream, pool: Arc<Mutex<Pool>>) {
    // Bound how long a client may hold a connection and how much we read, so a
    // slow-loris or a socket that never sends a newline cannot pin a thread or
    // grow memory without limit. The request line we care about is tiny.
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
    let Ok(peek) = s.try_clone() else { return };
    let mut reader = BufReader::new(peek.take(16 * 1024));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let target = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let params = parse_query(&query);
    // The standard miner API carries no worker id, so attribute by source IP.
    let peer = s
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let body = route(&path, &params, &pool, &peer);
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = s.write_all(resp.as_bytes());
    let _ = s.flush();
    // Orderly close: signal end-of-write, then drain any unread request bytes so
    // the peer's pending data does not RST-truncate our response (seen on Windows).
    let _ = s.shutdown(Shutdown::Write);
    let mut sink = [0u8; 2048];
    let _ = s.read(&mut sink);
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn route(
    path: &str,
    params: &HashMap<String, String>,
    pool: &Arc<Mutex<Pool>>,
    peer: &str,
) -> String {
    match path {
        // ---- standard Hacash miner API: an UNMODIFIED poworker mines here ----
        "/query/miner/pending" => plock(pool).pending_cache.clone(),
        "/query/miner/notice" => {
            let want: u64 = params.get("height").and_then(|v| v.parse().ok()).unwrap_or(0);
            let wait: u64 = params
                .get("wait")
                .and_then(|v| v.parse().ok())
                .unwrap_or(45)
                .clamp(1, 120);
            // Budget long-polls separately: if too many are already parked, answer
            // immediately with the current height rather than holding another slot.
            if NOTICE_WAITERS.fetch_add(1, Relaxed) >= MAX_NOTICE_WAITERS {
                NOTICE_WAITERS.fetch_sub(1, Relaxed);
                let h = plock(pool).tpl.height;
                return json!({"ret":0,"height":h}).to_string();
            }
            let _ng = NoticeGuard;
            let deadline = Instant::now() + Duration::from_secs(wait);
            loop {
                let h = plock(pool).tpl.height; // brief lock only
                if h > want || Instant::now() >= deadline {
                    return json!({"ret":0,"height":h}).to_string();
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        }
        "/submit/miner/success" => {
            let height: u64 = params.get("height").and_then(|v| v.parse().ok()).unwrap_or(0);
            let block_nonce: u32 = params
                .get("block_nonce")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let Some(cn) = parse32(params.get("coinbase_nonce")) else {
                return json!({"ret":1,"err":"bad coinbase_nonce"}).to_string();
            };
            // A share is only worth crediting if we can pay it. Require the miner
            // to announce a payable address; crediting an IP-fallback key that is
            // then dropped at payout would silently mine for nothing.
            let Some(worker) = params
                .get("worker")
                .filter(|w| is_payout_address(w))
                .cloned()
            else {
                return json!({
                    "ret": 1,
                    "err": "set pool_worker=<your HAC address> so the pool can pay you"
                })
                .to_string();
            };
            let _ = peer; // no longer used for attribution on the paid path
            let r = handle_submission(pool, &worker, height, cn, block_nonce);
            let ok = r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            let kind = r.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if ok {
                println!("[{worker}] {kind} at height {height}");
                json!({"ret":0,"kind":kind}).to_string()
            } else {
                json!({"ret":1,"kind":kind}).to_string()
            }
        }

        // ---- our own simple protocol (test-miner) ----
        "/work" => {
            let worker = params.get("worker").cloned().unwrap_or_else(|| "anon".into());
            let mut p = plock(pool);
            let en = p.extranonce_for(&worker);
            let cb = coinbase_with_extranonce(&p.tpl, &en);
            let intro = intro_bytes(&p.tpl, &cb, 0);
            json!({
                "ok": true,
                "height": p.tpl.height,
                "intro": hex::encode(intro),
                "share_target": hex::encode(p.share_target),
                "network_target": hex::encode(p.network_target),
                "extranonce": hex::encode(en),
            })
            .to_string()
        }
        "/share" => {
            let worker = params.get("worker").cloned().unwrap_or_else(|| "anon".into());
            let height: u64 = params.get("height").and_then(|v| v.parse().ok()).unwrap_or(0);
            let nonce: u32 = params.get("nonce").and_then(|v| v.parse().ok()).unwrap_or(0);
            let en = {
                let p = plock(pool);
                match p.workers.get(&worker).copied() {
                    Some(en) => en,
                    None => {
                        return json!({"ok":false,"kind":"invalid","err":"unknown worker"})
                            .to_string();
                    }
                }
            };
            handle_submission(pool, &worker, height, en, nonce).to_string()
        }
        "/stats" => {
            let p = plock(pool);
            json!({
                "height": p.tpl.height,
                "difficulty": p.tpl.difficulty,
                "accepted_shares": p.accepted,
                "blocks_confirmed": p.blocks,
                "blocks_pending": p.submitted.len(),
                "blocks_orphaned": p.orphaned,
                "share_window": p.pplns.total(),
                "workers": p.pplns.counts(),
            })
            .to_string()
        }
        _ => json!({"ok":false,"err":"no such endpoint"}).to_string(),
    }
}
