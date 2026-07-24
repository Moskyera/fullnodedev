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
//!   * a found block's coinbase is held back from settlement until the chain has
//!     buried it, so a reorg can never revoke income that was already paid out
//!   * only one process may settle a wallet (OS lock), and it shares ONE pending
//!     payout ledger with the manual pool-payout tool
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
//!   `chain` is REQUIRED: a wrong difficulty rule makes every share/block the
//!   node rejects, so there is no silent default. It is `mainnet`, `testnet`, or
//!   `testnet:<difficulty_adjust_blocks>:<each_block_target_time>` for a testnet
//!   node configured with anything other than the documented 288/10 pair. The
//!   choice is PROVED against the node's own tip before the pool serves work.

use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read, Write};
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
    PAYOUT_MATURITY_DEPTH, PPLNS_WINDOW, PayoutTxState, Template, acquire_settle_lock,
    assemble_block, atomic_write, balance, balance_units, block_reward_units, classify_payout_tx,
    coinbase_body_hex, coinbase_with_extranonce, distributable_units, fetch_template, find_str,
    find_u64, get_json, http_client, intro_bytes, is_payout_address, load_or_create_wallet,
    pool_state_path, post_hex, submit_block_bytes, verify_chain_params,
};

use serde_json::json;

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
/// How deep one of OUR blocks must be buried before its coinbase may be paid
/// out. The node treats anything shallower than `unstable_block` (4) as
/// reorg-able, and settlement runs at roughly one block interval, so without a
/// generous margin the pool would distribute income that is 0-1 confirmations
/// old. If such a block is later orphaned the income vanishes from the canonical
/// chain while the payout that spent it stays valid, and the operator eats the
/// whole subsidy with no way to recover it.
const COINBASE_MATURITY_DEPTH: u64 = 16;
/// Absolute wall-clock budget for reading a request line. A socket read timeout
/// bounds each read syscall, NOT the whole request, so a client dribbling one
/// byte at a time never trips it; this deadline is what actually stops it.
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(5);
/// Longest request line accepted. A miner API request line is tiny.
const MAX_REQUEST_LINE: usize = 4 * 1024;
/// Stack for a connection handler. HTTP parsing plus one x16rs evaluation needs
/// well under 100 KB, so this trims the address space reserved by MAX_CONNS
/// live handlers without coming close to the real requirement.
const HANDLER_STACK_BYTES: usize = 1024 * 1024;
/// Narrowest and widest share the pool will serve, as powers of two easier than
/// a network block. See `check_share_factor` for why both ends matter.
const MIN_SHARE_FACTOR: u32 = 18;
const MAX_SHARE_FACTOR: u32 = 40;

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

/// A serialized accounting snapshot on its way to disk. Building it needs the
/// pool lock; WRITING it must not hold it. Every served endpoint takes the same
/// mutex, so a create/rename/fsync on a slow, full or networked disk would
/// otherwise freeze work distribution and share acceptance for every miner for
/// as long as the disk takes.
struct StateShot {
    seq: u64,
    path: String,
    body: Vec<u8>,
    durable: bool,
}

/// Orders state writes and remembers the newest snapshot that reached disk, so a
/// writer that lost a race can never overwrite a fresher snapshot with a stale
/// one. Held only around the write itself, never together with the pool lock.
static PERSIST: LazyLock<Mutex<u64>> = LazyLock::new(|| Mutex::new(0));

/// Write a snapshot to disk, OFF the pool lock. The file is written atomically
/// (temp + optional fsync + rename) by `pool_spike::atomic_write`, so a crash or
/// a full disk mid-write can never leave a truncated or corrupt file.
///
/// Returns false only if this snapshot (or a newer one) did NOT reach disk, so a
/// caller about to move money can refuse to proceed rather than pay out
/// untracked.
fn flush_state(shot: Option<StateShot>) -> bool {
    let Some(shot) = shot else {
        return true;
    };
    let mut last = PERSIST.lock().unwrap_or_else(|e| e.into_inner());
    if shot.seq <= *last {
        // A newer snapshot already landed. It was taken under the pool lock after
        // this one, so it carries everything this one carried.
        return true;
    }
    if let Err(e) = atomic_write(&shot.path, &shot.body, shot.durable) {
        eprintln!("[state] save failed ({e}); accounting NOT flushed this round");
        return false;
    }
    *last = shot.seq;
    true
}

/// `durable` fsyncs before the rename; the frequent debounced share-save skips it
/// (a crash loses at most the last handful of shares, which is already the
/// accepted tolerance).
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
    /// Found blocks whose coinbase is NOT yet safe to distribute:
    /// (height, our hash, reward in units of 0.1 HAC). An entry leaves only once
    /// the chain still holds OUR hash COINBASE_MATURITY_DEPTH blocks later, or
    /// immediately once the chain shows a different hash there (orphaned, so
    /// that income never lands). Its reward is held back from `distributable`
    /// until then, and it is persisted so a restart cannot forget the hold-back.
    immature: Vec<(u64, [u8; 32], u64)>,
    /// Accepted shares not yet flushed to disk (debounces state writes).
    unsaved: u32,
    /// Monotonic snapshot counter: the state writer uses it to drop a snapshot
    /// that a fresher one has already overtaken.
    state_seq: u64,
    /// Hashes of payout transactions that have not yet confirmed. While ANY is
    /// still in the mempool we must not settle again (double spend). Persisted so
    /// a restart mid-settlement does not re-pay. Robust to a lost submit ACK and
    /// to the wallet also earning coinbase income.
    settle_pending_txs: Vec<String>,
}

impl Pool {
    /// Snapshot accounting for the disk at most every 16 shares. Block events
    /// snapshot directly, so a crash loses at worst a handful of shares. The
    /// caller must RELEASE the pool lock before handing the result to
    /// `flush_state`: this is the share hot path, and every other request is
    /// serialized behind this same mutex.
    fn note_share_saved(&mut self) -> Option<StateShot> {
        self.unsaved += 1;
        if self.unsaved < 16 {
            return None;
        }
        self.unsaved = 0;
        // Non-durable: high frequency, and losing <=16 shares on a crash is
        // already the accepted tolerance. Block/settle events fsync.
        self.state_shot(false)
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

    /// Serialize the accounting for `flush_state` to write once the pool lock is
    /// released. `durable` fsyncs before the rename (block-found and settlement
    /// events); the debounced share-save does not.
    fn state_shot(&mut self, durable: bool) -> Option<StateShot> {
        if self.state_file.is_empty() {
            return None;
        }
        let body = json!({
            "window": PPLNS_WINDOW,
            "order": self.pplns.snapshot(),
            "accepted": self.accepted,
            "blocks": self.blocks,
            "orphaned": self.orphaned,
            "settle_pending_txs": self.settle_pending_txs,
            // Blocks still awaiting confirmation. Without these a restart in the
            // window between finding a block and burying it drops it from the
            // confirm/orphan reconciliation for good, so a later reorg of one of
            // OUR blocks is never detected and the operator's stats drift.
            "submitted": self.submitted.iter().map(|(h, hash)| json!({
                "height": h,
                "hash": hex::encode(hash),
            })).collect::<Vec<_>>(),
            "immature": self.immature.iter().map(|(h, hash, u)| json!({
                "height": h,
                "hash": hex::encode(hash),
                "units": u,
            })).collect::<Vec<_>>(),
        });
        self.state_seq += 1;
        Some(StateShot {
            seq: self.state_seq,
            path: self.state_file.clone(),
            body: body.to_string().into_bytes(),
            durable,
        })
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
        // Blocks awaiting confirmation must survive a restart too, or one of OUR
        // blocks being orphaned goes unnoticed and blocks_confirmed permanently
        // over-counts against what the chain actually holds.
        self.submitted = j
            .get("submitted")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        let h = x.get("height").and_then(|v| v.as_u64())?;
                        let hash = hash32(x.get("hash").and_then(|v| v.as_str())?)?;
                        Some((h, hash))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The hold-back must survive a restart: forgetting it would let the very
        // next settle cycle distribute income a reorg can still revoke.
        self.immature = j
            .get("immature")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        let h = x.get("height").and_then(|v| v.as_u64())?;
                        let u = x.get("units").and_then(|v| v.as_u64())?;
                        let hash = hash32(x.get("hash").and_then(|v| v.as_str())?)?;
                        Some((h, hash, u))
                    })
                    .collect()
            })
            .unwrap_or_default();
        println!(
            "restored accounting: {} shares in window, {} blocks, {} orphaned, \
             {} block(s) awaiting confirmation, {} payout(s) pending, \
             {} block(s) of income not yet matured",
            self.pplns.total(),
            self.blocks,
            self.orphaned,
            self.submitted.len(),
            self.settle_pending_txs.len(),
            self.immature.len()
        );
    }
}

/// Refuse a share size that would make the equal-weight PPLNS window unfair.
///
/// Every accepted share is credited with weight 1, and the network difficulty in
/// force when it was mined is not recorded. That is exact only while the whole
/// window is far SHORTER than one block interval, because difficulty moves only
/// at a block boundary: with 2^factor shares to a block, PPLNS_WINDOW shares
/// span PPLNS_WINDOW / 2^factor of a block - 0.02% at the default 24, still only
/// 1.6% at 18. Go lower and a difficulty change lands inside a live window, so
/// real payout money is split by share counts that stand for different amounts
/// of work. The upper bound keeps a share from being so easy that a whole GPU
/// batch always beats it (credit would then track batch cadence, not hashrate)
/// and that the share target degenerates into the all-0xff ceiling.
fn check_share_factor(factor: u32) -> Result<(), String> {
    if !(MIN_SHARE_FACTOR..=MAX_SHARE_FACTOR).contains(&factor) {
        return Err(format!(
            "share_bits must be between {MIN_SHARE_FACTOR} and {MAX_SHARE_FACTOR} (got {factor}).\n\
             A share is 2^share_bits easier than a network block; below {MIN_SHARE_FACTOR} the \
             {PPLNS_WINDOW}-share payout window covers enough of a block interval that a \
             difficulty change inside it would misallocate real payouts."
        ));
    }
    Ok(())
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
    if let Err(e) = check_share_factor(share_factor) {
        eprintln!("{e}");
        std::process::exit(2);
    }
    // chain is REQUIRED: a mainnet pool run with testnet difficulty (or vice
    // versa) computes the wrong target and every block/share is rejected. Refuse
    // to guess.
    let Some(chain) = a.get(5).cloned() else {
        eprintln!(
            "usage: pool-server <node> <wallet_file> <listen> <share_bits> <chain> [settle_secs]\n\
             `chain` is required: `mainnet`, `testnet`, or \
             `testnet:<difficulty_adjust_blocks>:<each_block_target_time>`."
        );
        std::process::exit(2);
    };
    // A testnet node takes its difficulty window and block time from its OWN
    // config file, so accept them spelled out rather than assuming a pair that
    // would make the node reject every block this pool mines.
    let Some(params) = ChainParams::parse(&chain) else {
        eprintln!(
            "chain must be `mainnet`, `testnet`, or \
             `testnet:<difficulty_adjust_blocks>:<each_block_target_time>` (got `{chain}`)"
        );
        std::process::exit(2);
    };
    let settle_secs: u64 = a.get(6).and_then(|s| s.parse().ok()).unwrap_or(300);

    println!("== pool-server ==");
    println!("node    = {node}");
    // Exactly one process may settle a wallet, enforced by the OS for as long as
    // this one lives. `pool-payout` takes the SAME lock, so it can never pay out
    // of a wallet this server is already settling: both read the CONFIRMED
    // balance (a payout waiting in the mempool does not reduce it), so each would
    // see the full balance and pay the same PPLNS window a second time.
    let _settle_lock = match acquire_settle_lock(&wallet_file) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "another pool-server or pool-payout already holds {wallet_file} ({e}).\n\
                 Only one process may settle a wallet - stop the other one first."
            );
            std::process::exit(2);
        }
    };
    let wallet = load_or_create_wallet(&wallet_file);
    let payout = wallet.readable().to_string();

    let client = http_client();
    // Prove the difficulty rule in force here reproduces the node's OWN tip
    // before serving a single piece of work. Otherwise a chain label that does
    // not match the node's config makes every block the pool finds rejected, and
    // nothing says so: the pool just mines dead work indefinitely.
    if let Err(e) = verify_chain_params(&client, &node, &params) {
        eprintln!("REFUSING to start: {e}");
        std::process::exit(2);
    }
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
        state_file: pool_state_path(&wallet_file),
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
        immature: Vec::new(),
        unsaved: 0,
        state_seq: 0,
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
        // NEVER `thread::spawn` here: it is `Builder::spawn(..).expect(..)`, so it
        // PANICS when the OS refuses a new thread (RLIMIT_NPROC, exhausted address
        // space), and that panic unwinds straight out of main and kills the whole
        // pool. Treat a spawn failure as backpressure instead: dropping the
        // closure releases the ConnGuard (global + per-IP slot) and closes `s`.
        if let Err(e) = std::thread::Builder::new()
            .stack_size(HANDLER_STACK_BYTES)
            .spawn(move || {
                let _g = guard; // releases the slot on return AND on unwind
                handle(s, p);
            })
        {
            eprintln!("[accept] thread spawn failed, dropping connection: {e}");
        }
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
    let (pending, immature) = {
        let p = plock(pool);
        (p.submitted.clone(), p.immature.clone())
    };
    let tip = fresh.as_ref().map(|t| t.height.saturating_sub(1));
    // One node query per height, shared by the confirm/orphan tally and by the
    // coinbase-maturity gate below.
    let mut heights: Vec<u64> = pending
        .iter()
        .map(|(h, _)| *h)
        .chain(immature.iter().map(|(h, _, _)| *h))
        .collect();
    heights.sort_unstable();
    heights.dedup();
    let mut chain_hash: HashMap<u64, String> = HashMap::new();
    for h in heights {
        if tip.map(|t| h > t).unwrap_or(true) {
            continue; // not buried yet, or no tip this cycle
        }
        let j = get_json(client, &format!("{node}/query/block/intro?height={h}"));
        if let Some(hx) = find_str(&j, "hash") {
            chain_hash.insert(h, hx);
        }
    }
    // A block counts as confirmed only once the chain has BURIED it while still
    // holding our hash. Finalizing it the moment it merely occupies the tip (0
    // blocks stacked on top) also stops us watching it - exactly when a reorg is
    // most likely - so an orphan after that point could never be detected and
    // blocks_confirmed would over-count against the chain for good.
    let mut confirmed = Vec::new();
    let mut orphaned = Vec::new();
    for (h, ours) in &pending {
        match chain_hash.get(h) {
            Some(cur) if *cur == hex::encode(ours) => {
                if buried_deep(tip, *h) {
                    confirmed.push((*h, *ours));
                }
                // Not buried yet: keep watching it, a reorg can still flip it.
            }
            Some(cur) => {
                orphaned.push((*h, *ours));
                println!("[reorg] our block {h} orphaned (chain holds {cur})");
            }
            None => {} // node has not stored it yet; keep waiting
        }
    }
    // Coinbase maturity: a found block's reward stays held back until the chain
    // still holds OUR hash COINBASE_MATURITY_DEPTH blocks later. Releasing it any
    // earlier means the pool can pay out income that a reorg then takes back,
    // while the payout transaction that spent it stays valid on the new chain.
    let mut released: Vec<(u64, [u8; 32], u64)> = Vec::new();
    for (h, ours, u) in &immature {
        match chain_hash.get(h) {
            Some(cur) if *cur == hex::encode(ours) => {
                if buried_deep(tip, *h) {
                    released.push((*h, *ours, *u));
                }
            }
            // Orphaned: that income never lands in the balance, so there is
            // nothing left to hold back.
            Some(_) => released.push((*h, *ours, *u)),
            None => {}
        }
    }
    let mut shot = None;
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
                let height = p.tpl.height;
                prune_seen(&mut p.seen, height);
                p.rebuild_pending_cache();
            }
        }
        p.blocks += confirmed.len() as u64;
        p.orphaned += orphaned.len() as u64;
        p.submitted
            .retain(|e| !confirmed.contains(e) && !orphaned.contains(e));
        if !released.is_empty() {
            p.immature.retain(|e| !released.contains(e));
        }
        // Block bookkeeping changed: snapshot it here, write it below with the
        // lock released.
        if !confirmed.is_empty() || !orphaned.is_empty() || !released.is_empty() {
            shot = p.state_shot(true);
        }
    }
    flush_state(shot);
}

/// Has the chain stacked COINBASE_MATURITY_DEPTH blocks on top of height `h`?
/// A `None` tip means we could not read the chain this cycle, so nothing counts
/// as buried: both callers must err towards keeping a block under observation.
fn buried_deep(tip: Option<u64>, h: u64) -> bool {
    tip.map(|t| t.saturating_sub(h) >= COINBASE_MATURITY_DEPTH)
        .unwrap_or(false)
}

/// Drop replay-protection entries that can no longer be resubmitted (strictly
/// lower heights) and KEEP every entry at the current height.
///
/// Clearing the whole set on a template swap looks harmless because a swap
/// usually means a new height, but it also fires on a SAME-height reorg. If the
/// chain then flaps back to the original prev-hash the template is byte-identical
/// again, and an emptied set re-admits solutions that were already credited -
/// letting a miner double-count their own shares in the PPLNS window and take
/// payout funds from everyone else.
fn prune_seen(seen: &mut HashSet<(u64, [u8; 32], u32)>, height: u64) {
    seen.retain(|(h, _, _)| *h >= height);
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
    //
    // This guard must fail SAFE. Only a definitive node verdict resolves a hash:
    // an unreachable node, a timeout or an answer we cannot parse keeps the hash
    // and skips the cycle, and a payout confirmed shallower than
    // PAYOUT_MATURITY_DEPTH stays tracked because a reorg could still return it
    // to the mempool. Anything else re-opens the double-payout window.
    if !pending_txs.is_empty() {
        let mut still = Vec::new();
        for hx in &pending_txs {
            let j = get_json(&client, &format!("{node}/query/transaction?hash={hx}"));
            // Never slice mid-character: these come off disk, and a corrupt
            // ledger entry must not panic the settlement thread.
            let short = hx.get(..16).unwrap_or(hx);
            match classify_payout_tx(&j) {
                PayoutTxState::Buried(_) => {}
                PayoutTxState::Gone => eprintln!(
                    "[settle] payout tx {short} is unknown to the node (rejected or dropped); \
                     this cycle will re-issue it"
                ),
                PayoutTxState::Pending => still.push(hx.clone()),
                PayoutTxState::Confirming(d) => {
                    println!(
                        "[settle] payout tx {short} is only {d} block(s) deep; \
                         waiting for {PAYOUT_MATURITY_DEPTH} before settling again"
                    );
                    still.push(hx.clone());
                }
                PayoutTxState::Unknown => {
                    eprintln!(
                        "[settle] could not determine the state of payout tx {short}; \
                         keeping it and skipping this cycle"
                    );
                    still.push(hx.clone());
                }
            }
        }
        if !still.is_empty() {
            // Some payout is still in flight (or unresolved); keep those and skip.
            let shot = {
                let mut p = plock(pool);
                p.settle_pending_txs = still;
                p.state_shot(true)
            };
            flush_state(shot);
            return;
        }
        // Every prior payout is buried or definitively gone: clear and settle
        // fresh income.
        let shot = {
            let mut p = plock(pool);
            p.settle_pending_txs.clear();
            p.state_shot(true)
        };
        flush_state(shot);
    }

    let acc = load_or_create_wallet(wallet_file);
    let bal = balance(&client, &node, acc.readable());
    // An answer we cannot value is NOT a zero balance: paying out on a garbled or
    // implausible one would sign transactions for a number the node never
    // reported. Skip the cycle instead; the accounting is untouched.
    let Some(units) = balance_units(&bal) else {
        eprintln!(
            "[settle] the node reported a balance this pool cannot value ({bal:?}); \
             skipping this settlement cycle"
        );
        return;
    };
    // Read the hold-back AFTER the balance, never before. A block found in
    // between then appears in the hold-back but not yet in the balance, which
    // errs towards paying LESS; the other order would let a block found during
    // the (slow) pending-payout poll slip through and be paid at 0 confirmations.
    let immature_units: u64 = plock(pool).immature.iter().map(|(_, _, u)| *u).sum();

    // Keep a reserve so the wallet always covers the (per-chunk) tx fee. No pool
    // fee is skimmed: this is a community pool, and the reserve covers the fees.
    let reserve = 5u64; // 0.5 HAC — covers up to ~50 chunk fees of 0.01 HAC each
    // Hold back the coinbase of blocks that are not yet buried: distributing
    // income a reorg can still revoke costs the operator a whole subsidy that
    // nothing can claw back, because the payout stays valid on the new chain.
    let Some(distributable) = distributable_units(units, immature_units, reserve) else {
        if immature_units > 0 {
            println!(
                "[settle] holding back {immature_units} unit(s) of block income that is not yet \
                 buried {COINBASE_MATURITY_DEPTH} deep; nothing matured to pay this cycle"
            );
        }
        return;
    };

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
        let shot = {
            let mut p = plock(pool);
            p.settle_pending_txs.push(txhash.clone());
            p.state_shot(true)
        };
        // The fsync happens here, with the pool lock released: no miner request
        // waits on the state disk, and nothing is submitted before it lands.
        let recorded = flush_state(shot);
        if !recorded {
            // An untracked payout is one a later cycle could pay all over again.
            // Nothing was submitted, so stopping here loses nothing; the hash
            // stays in memory and next cycle's poll resolves it as gone.
            eprintln!(
                "[settle] could not record the payout tx on disk; NOT submitting this chunk \
                 (an untracked payout could be paid twice)"
            );
            break;
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
    // No route may seat an unpayable key in the PPLNS window. The window is a
    // fixed 4096 shares shared by everyone, so a key that is filtered out at
    // payout still evicts shares from miners the pool CAN pay - their work is
    // credited for a shorter stretch and small/sporadic miners fall out of the
    // window before a block is found. The paid path already refuses these; this
    // is the backstop for every other caller.
    if !is_payout_address(worker) {
        return json!({
            "ok": false,
            "kind": "invalid",
            "err": "set worker=<your HAC address> so the pool can pay you"
        });
    }
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
    // The accounting snapshot leaves the lock as bytes; writing it is phase 3b,
    // because every other request is serialized behind this same mutex and a
    // create/rename/fsync must never happen underneath it.
    let (commit, shot) = {
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
            (Commit::Share(p.accepted), p.note_share_saved())
        } else {
            let bytes = assemble_block(&tpl, &cb, block_nonce);
            let solved = tpl.height;
            p.submitted.push((solved, hash)); // counted once the bg thread sees it stick
            // Hold this block's coinbase back from settlement until the chain has
            // buried it. The node credits the reward the moment the block is
            // inserted, so without this the very next settle tick would pay out
            // income that is 0-1 confirmations deep.
            p.immature.push((solved, hash, block_reward_units(solved)));
            (Commit::Block(bytes), p.state_shot(true))
        }
    };

    // Phase 3b - no lock: persist the accounting (fsync on a block) before the
    // block goes out, so a crash right after submitting still knows about it.
    flush_state(shot);

    let block_bytes = match commit {
        Commit::Share(accepted) => {
            return json!({"ok":true,"kind":"share","accepted":accepted});
        }
        Commit::Block(bytes) => bytes,
    };

    // Phase 4 — no lock: submit the winning block.
    let submit = submit_block_bytes(&client, &node, &block_bytes);
    json!({"ok":true,"kind":"block","solved_height":height,"submit":submit})
}

/// What phase 3 of a submission decided, carried out of the lock scope so the
/// answer is built (and the state written) with the pool mutex released.
enum Commit {
    Share(u64),
    Block(Vec<u8>),
}

fn hash32(s: &str) -> Option<[u8; 32]> {
    let v = hex::decode(s).ok()?;
    if v.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Some(out)
}

fn parse32(s: Option<&String>) -> Option<[u8; 32]> {
    hash32(s?)
}

/// Read the HTTP request line under BOTH a size cap and an ABSOLUTE wall-clock
/// deadline.
///
/// A socket read timeout bounds each read syscall, not the request: a client
/// dribbling one byte every few seconds resets the timer on every byte and can
/// hold a handler thread (plus its global and per-IP slot) for hours, which is
/// all a slow-loris needs to starve every real miner. Checking elapsed time
/// after each read is what actually bounds it.
fn read_request_line(s: TcpStream) -> Option<String> {
    let start = Instant::now();
    let mut reader = BufReader::new(s.take(MAX_REQUEST_LINE as u64));
    let mut line: Vec<u8> = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        if start.elapsed() >= REQUEST_READ_DEADLINE {
            return None;
        }
        match reader.read(&mut byte) {
            Ok(0) => return None, // peer closed before sending a full line
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                if line.len() >= MAX_REQUEST_LINE {
                    return None;
                }
                line.push(byte[0]);
            }
            // A per-read timeout is not fatal on its own; the deadline above is
            // what ends the connection. Any other error is.
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return None,
        }
    }
    String::from_utf8(line).ok()
}

fn handle(mut s: TcpStream, pool: Arc<Mutex<Pool>>) {
    // Bound how long a client may hold a connection and how much we read, so a
    // slow-loris or a socket that never sends a newline cannot pin a thread or
    // grow memory without limit. The request line we care about is tiny. The
    // per-read timeout is short so the absolute deadline is honoured promptly.
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
    let Ok(peek) = s.try_clone() else { return };
    let Some(line) = read_request_line(peek) else {
        return;
    };
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
        // Both routes demand a payable worker exactly like the paid path: a share
        // credited to a key the pool cannot pay is work done for nothing that
        // also evicts a payable miner's share from the shared 4096-share window.
        "/work" => {
            let Some(worker) = params.get("worker").filter(|w| is_payout_address(w)).cloned()
            else {
                return json!({
                    "ok": false,
                    "err": "set worker=<your HAC address> so the pool can pay you"
                })
                .to_string();
            };
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
            let Some(worker) = params.get("worker").filter(|w| is_payout_address(w)).cloned()
            else {
                return json!({
                    "ok": false,
                    "kind": "invalid",
                    "err": "set worker=<your HAC address> so the pool can pay you"
                })
                .to_string();
            };
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
            // Copy the numbers out, then RELEASE the lock before building the
            // body. /stats is open and unauthenticated, and serializing up to
            // PPLNS_WINDOW worker rows under the global mutex would let anyone
            // stall every miner's /work, /share and /submit by polling it.
            let (height, difficulty, accepted, blocks, pending, orphaned, window, workers) = {
                let p = plock(pool);
                (
                    p.tpl.height,
                    p.tpl.difficulty,
                    p.accepted,
                    p.blocks,
                    p.submitted.len(),
                    p.orphaned,
                    p.pplns.total(),
                    p.pplns.counts(),
                )
            };
            json!({
                "height": height,
                "difficulty": difficulty,
                "accepted_shares": accepted,
                "blocks_confirmed": blocks,
                "blocks_pending": pending,
                "blocks_orphaned": orphaned,
                "share_window": window,
                "workers": workers,
            })
            .to_string()
        }
        _ => json!({"ok":false,"err":"no such endpoint"}).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_same_height_reorg_keeps_already_credited_solutions() {
        // Clearing `seen` on a same-height template swap let an A -> B -> A flap
        // re-admit solutions that were already credited, doubling their PPLNS
        // weight at every honest miner's expense.
        let mut seen: HashSet<(u64, [u8; 32], u32)> = HashSet::new();
        seen.insert((100, [7u8; 32], 42));
        seen.insert((100, [8u8; 32], 43));
        seen.insert((99, [7u8; 32], 1));
        prune_seen(&mut seen, 100); // same height, different prev-hash
        assert!(seen.contains(&(100, [7u8; 32], 42)));
        assert!(seen.contains(&(100, [8u8; 32], 43)));
        assert!(!seen.contains(&(99, [7u8; 32], 1)), "stale heights are pruned");
        // A height advance drops everything: those keys fail the freshness check
        // anyway, so keeping them would only grow memory.
        prune_seen(&mut seen, 101);
        assert!(seen.is_empty());
    }

    #[test]
    fn a_block_is_only_confirmed_once_the_chain_has_buried_it() {
        // Counting a block the moment it merely occupies the tip also stopped us
        // watching it, so a shallow reorg of one of OUR blocks could never be
        // detected and blocks_confirmed over-counted for good.
        let h = 1_000u64;
        assert!(!buried_deep(Some(h), h), "0 blocks stacked on top is not buried");
        assert!(!buried_deep(Some(h + COINBASE_MATURITY_DEPTH - 1), h));
        assert!(buried_deep(Some(h + COINBASE_MATURITY_DEPTH), h));
        // No tip this cycle: keep watching rather than finalize on a guess.
        assert!(!buried_deep(None, h));
    }

    #[test]
    fn share_size_that_would_unbalance_the_payout_window_is_refused() {
        // Shares are credited with equal weight, which is only fair while the
        // whole window is much shorter than one block interval.
        assert!(check_share_factor(24).is_ok()); // documented default
        assert!(check_share_factor(MIN_SHARE_FACTOR).is_ok());
        assert!(check_share_factor(MAX_SHARE_FACTOR).is_ok());
        assert!(check_share_factor(MIN_SHARE_FACTOR - 1).is_err());
        assert!(check_share_factor(0).is_err());
        assert!(check_share_factor(MAX_SHARE_FACTOR + 1).is_err());
    }

    #[test]
    fn immature_block_income_is_not_distributable() {
        // Two found blocks are still shallow, so their subsidy must stay out of
        // the payout even though the node already credited it to the wallet.
        let immature = [
            (900u64, [1u8; 32], block_reward_units(900)),
            (901u64, [2u8; 32], block_reward_units(901)),
        ];
        let held: u64 = immature.iter().map(|(_, _, u)| *u).sum();
        assert!(held > 0);
        let reserve = 5u64;
        // Balance is exactly the two fresh subsidies plus the reserve: nothing
        // has matured, so the pool must pay nothing at all.
        assert_eq!(distributable_units(held + reserve, held, reserve), None);
        // Once one of them buries, only that block's income is released.
        let matured_one = immature[0].2;
        assert_eq!(
            distributable_units(held + reserve, held - matured_one, reserve),
            Some(matured_one)
        );
    }
}
