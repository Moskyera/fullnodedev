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
//!   * accounting is persisted, so a restart never erases credited work
//!   * a submitted block only counts once the chain still holds OUR hash at that
//!     height — orphans are detected and not paid for
//!   * settlement runs automatically on a timer
//!
//! Endpoints: /work, /share, /stats (own protocol) and /query/miner/pending,
//! /query/miner/notice, /submit/miner/success (standard API).
//!
//! Usage: pool-server [node] [wallet_file] [listen] [share_bits] [chain] [settle_secs]

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use basis::interface::*;
use field::*;
use protocol::action::HacToTrs;
use protocol::transaction::TransactionType2;
use sys::curtimes;

use pool_spike::difficulty::ChainParams;
use pool_spike::pool_core::{self, Pplns, split_payout};
use pool_spike::{
    Template, assemble_block, balance, coinbase_body_hex, coinbase_with_extranonce, fetch_template,
    find_str, find_u64, get_json, http_client, intro_bytes, is_payout_address,
    load_or_create_wallet, post_hex, submit_block_bytes,
};

use serde_json::json;

const PPLNS_WINDOW: usize = 4096;

struct Pool {
    node: String,
    payout: String,
    state_file: String,
    client: reqwest::blocking::Client,
    params: ChainParams,
    tpl: Template,
    share_target: [u8; 32],
    network_target: [u8; 32],
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
    /// The hash of the last payout transaction that has not yet confirmed. While
    /// it is still in the mempool we must not settle again (double spend); we
    /// clear it only once the node reports it confirmed or gone. This is robust
    /// to a lost submit ACK and to the wallet also earning coinbase income.
    settle_pending_tx: Option<String>,
}

impl Pool {
    /// Flush accounting to disk at most every 16 shares. Block events call
    /// save_state directly, so a crash loses at worst a handful of shares.
    fn note_share_saved(&mut self) {
        self.unsaved += 1;
        if self.unsaved >= 16 {
            self.save_state();
            self.unsaved = 0;
        }
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

    fn save_state(&self) {
        if self.state_file.is_empty() {
            return;
        }
        let body = json!({
            "window": PPLNS_WINDOW,
            "order": self.pplns.snapshot(),
            "accepted": self.accepted,
            "blocks": self.blocks,
            "orphaned": self.orphaned,
        });
        let _ = std::fs::write(&self.state_file, body.to_string());
    }

    fn load_state(&mut self) {
        let Ok(txt) = std::fs::read_to_string(&self.state_file) else {
            return;
        };
        let Ok(j) = serde_json::from_str::<serde_json::Value>(&txt) else {
            return;
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
        println!(
            "restored accounting: {} shares in window, {} blocks, {} orphaned",
            self.pplns.total(),
            self.blocks,
            self.orphaned
        );
    }
}

/// A target requiring `bits` leading zero bits (the pool difficulty knob).
fn target_leading_zero_bits(bits: u32) -> [u8; 32] {
    let mut t = [0xffu8; 32];
    let full = (bits / 8) as usize;
    let rem = bits % 8;
    for b in t.iter_mut().take(full.min(32)) {
        *b = 0x00;
    }
    if full < 32 && rem > 0 {
        t[full] = 0xffu8 >> rem;
    }
    t
}

/// A node "mantissa:unit" balance expressed in units of 0.1 HAC (unit 247).
///
/// Hacash stores amounts normalized (trailing zeros stripped, unit raised), so a
/// balance like 4.9 HAC comes back as "49:246", not "490:247". We must FLOOR to
/// 0.1-HAC granularity, keeping the whole part, rather than discarding a balance
/// just because it is finer than 0.1 HAC (that used to freeze all payouts once
/// the wallet held any sub-0.1-HAC change).
fn balance_units(bal: &str) -> u64 {
    let Some((m, u)) = bal.split_once(':') else {
        return 0;
    };
    let (Ok(m), Ok(u)) = (m.trim().parse::<u64>(), u.trim().parse::<i64>()) else {
        return 0;
    };
    if u >= 247 {
        let exp = (u - 247) as u32;
        if exp > 18 {
            return u64::MAX;
        }
        m.saturating_mul(10u64.pow(exp))
    } else {
        // Finer than 0.1 HAC: floor to whole 0.1-HAC units, keeping the value.
        let exp = (247 - u) as u32;
        if exp > 18 {
            return 0; // finer than 1 zhu is impossible; nothing to keep
        }
        m / 10u64.pow(exp)
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
    let share_bits: u32 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);
    let chain = a.get(5).cloned().unwrap_or_else(|| "testnet".to_string());
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
    println!("share   = {share_bits} leading zero bits");
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
        tpl,
        share_target: target_leading_zero_bits(share_bits),
        network_target,
        workers: HashMap::new(),
        next_en: 0,
        pplns: Pplns::new(PPLNS_WINDOW),
        accepted: 0,
        blocks: 0,
        orphaned: 0,
        seen: HashSet::new(),
        submitted: Vec::new(),
        unsaved: 0,
        settle_pending_tx: None,
    };
    pool.load_state();
    let pool = Arc::new(Mutex::new(pool));

    // Background: keep the template current with the chain tip and confirm our
    // submitted blocks. All node HTTP happens OFF the pool lock, so miners are
    // never stalled by it. This also advances work when the NETWORK finds a
    // block, not only when we do.
    {
        let pool = pool.clone();
        let (client, node, payout, params) = {
            let p = pool.lock().unwrap();
            (p.client.clone(), p.node.clone(), p.payout.clone(), p.params.clone())
        };
        std::thread::spawn(move || {
            loop {
                let fresh = fetch_template(&client, &node, &payout, &params);
                let pending: Vec<(u64, [u8; 32])> = pool.lock().unwrap().submitted.clone();
                let tip = fresh.as_ref().map(|t| t.height.saturating_sub(1));
                let mut confirmed = Vec::new();
                let mut orphaned = Vec::new();
                for (h, ours) in &pending {
                    if tip.map(|t| *h > t).unwrap_or(true) {
                        continue; // not buried yet, or no tip this cycle
                    }
                    let j = get_json(&client, &format!("{node}/query/block/intro?height={h}"));
                    match find_str(&j, "hash") {
                        Some(chain_hash) if chain_hash == hex::encode(ours) => {
                            confirmed.push((*h, *ours))
                        }
                        Some(chain_hash) => {
                            orphaned.push((*h, *ours));
                            println!("[reorg] our block {h} orphaned (chain holds {chain_hash})");
                        }
                        None => {} // node has not stored it yet; keep waiting
                    }
                }
                {
                    let mut p = pool.lock().unwrap();
                    if let Some(t) = fresh {
                        // Replace the template ONLY when the height advances. At
                        // the same height the timestamp (and thus the ASERT
                        // difficulty) is fixed, so keeping it valid keeps every
                        // worker's in-flight share valid; recomputing it would
                        // reject work the pool just handed out.
                        if t.height != p.tpl.height {
                            p.tpl = t;
                            p.network_target = p.tpl.target;
                            p.seen.clear();
                        }
                    }
                    p.blocks += confirmed.len() as u64;
                    p.orphaned += orphaned.len() as u64;
                    p.submitted
                        .retain(|e| !confirmed.contains(e) && !orphaned.contains(e));
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
    // Cap live connections so unauthenticated long-poll / slow clients cannot
    // spawn unbounded threads.
    let conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    const MAX_CONNS: usize = 1024;
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if conns.load(std::sync::atomic::Ordering::Relaxed) >= MAX_CONNS {
                    continue; // drop: s closes as it goes out of scope
                }
                conns.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let p = pool.clone();
                let c = conns.clone();
                std::thread::spawn(move || {
                    handle(s, p);
                    c.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
}

/// Pay every miner their PPLNS share of the pool's spendable balance, in ONE
/// signed transaction submitted to the node's mempool.
fn settle_once(pool: &Arc<Mutex<Pool>>, wallet_file: &str) {
    let (node, counts, pending_tx) = {
        let p = pool.lock().unwrap();
        (p.node.clone(), p.pplns.counts(), p.settle_pending_tx.clone())
    };
    if counts.is_empty() {
        return;
    }
    let client = http_client();

    // Resolve any outstanding payout FIRST, using the node's own view of the tx
    // rather than the wallet balance. This is correct even if a submit ACK was
    // lost and even though the same wallet keeps earning coinbase income.
    if let Some(hx) = pending_tx {
        let j = get_json(&client, &format!("{node}/query/transaction?hash={hx}"));
        let ret_ok = find_u64(&j, "ret") == Some(0);
        let still_pending = j
            .get("data")
            .and_then(|d| d.get("pending"))
            .and_then(|v| v.as_bool())
            .or_else(|| j.get("pending").and_then(|v| v.as_bool()))
            .unwrap_or(false);
        if ret_ok && still_pending {
            return; // last payout is still in the mempool; do not settle again
        }
        // Either confirmed on-chain (ret ok, not pending) or gone (not found):
        // clear the marker and allow a fresh settle of new income.
        pool.lock().unwrap().settle_pending_tx = None;
    }

    let acc = load_or_create_wallet(wallet_file);
    let bal = balance(&client, &node, acc.readable());
    let units = balance_units(&bal);

    // Keep a reserve so the wallet always has enough for the tx fee. No pool fee
    // is skimmed: this is a community pool, and the reserve covers the tiny fee.
    let reserve = 5u64; // 0.5 HAC
    if units <= reserve + 1 {
        return;
    }
    let distributable = units - reserve;
    let split = split_payout(distributable, 0, 1, &counts);
    let payable: Vec<(String, u64)> = split
        .into_iter()
        .filter(|(w, _)| is_payout_address(w))
        .collect();
    if payable.is_empty() {
        return;
    }

    let main = Address::from(*acc.address());
    let fee = Amount::from("1:246").expect("fee"); // 0.01 HAC tx fee (from reserve)
    let mut tx = TransactionType2::new_by(main, fee, curtimes());
    for (addr, u) in &payable {
        let to = Address::from_readable(addr).expect("payout address");
        let amt = Amount::from(&format!("{u}:247")).expect("amount");
        let mut act = HacToTrs::new();
        act.to = AddrOrPtr::from_addr(to);
        act.hacash = amt;
        if tx.push_action(Box::new(act)).is_err() {
            break;
        }
    }
    if tx.fill_sign(&acc).is_err() {
        println!("[settle] signing failed");
        return;
    }
    // Record the payout tx hash BEFORE submitting, so a lost ACK still blocks a
    // second settle: next cycle we poll this hash and only retry if it is gone.
    let txhash = hex::encode(tx.hash().serialize());
    pool.lock().unwrap().settle_pending_tx = Some(txhash.clone());

    let body = hex::encode(tx.serialize());
    let resp = post_hex(
        &client,
        &format!("{node}/submit/transaction?hexbody=true"),
        &body,
    );
    println!(
        "[settle] paid {} miner(s) {} units, tx {} -> {resp}",
        payable.len(),
        distributable,
        &txhash[..16]
    );
}

enum Credit {
    Reject(serde_json::Value),
    Share(serde_json::Value),
    /// A network-target solution: the full block bytes to submit (off-lock).
    Block { block_bytes: Vec<u8>, solved: u64 },
}

/// The locked, NO-network-I/O part: validate a solution and credit it. On a
/// full block it assembles the bytes (pure CPU) and records it as pending; the
/// actual submit happens off-lock in handle_submission, and the background
/// thread confirms it. This keeps blocking node calls out of the critical
/// section so one submission cannot stall every other miner.
fn credit_share(
    p: &mut Pool,
    worker: &str,
    height: u64,
    coinbase_nonce: [u8; 32],
    block_nonce: u32,
) -> Credit {
    if height != p.tpl.height {
        return Credit::Reject(json!({"ok":false,"kind":"stale","height":p.tpl.height}));
    }
    // Reject replays BEFORE crediting: the same solution must never be counted
    // twice, or a miner could inflate its share of the payout.
    let key = (height, coinbase_nonce, block_nonce);
    if p.seen.contains(&key) {
        return Credit::Reject(json!({"ok":false,"kind":"duplicate"}));
    }
    // Rebuild exactly what the worker hashed.
    let cb = coinbase_with_extranonce(&p.tpl, &coinbase_nonce);
    let intro = intro_bytes(&p.tpl, &cb, block_nonce);
    let hash = pool_core::hash_of(p.tpl.height, &intro);
    if !pool_core::beats(&hash, &p.share_target) {
        return Credit::Reject(json!({"ok":false,"kind":"invalid","err":"above share target"}));
    }

    p.seen.insert(key);
    p.pplns.record(worker);
    p.accepted += 1;

    if !pool_core::beats(&hash, &p.network_target) {
        p.note_share_saved();
        return Credit::Share(json!({"ok":true,"kind":"share","accepted":p.accepted}));
    }

    let block_bytes = assemble_block(&p.tpl, &cb, block_nonce);
    let solved = p.tpl.height;
    p.submitted.push((solved, hash)); // counted once the background thread sees it stick
    p.save_state();
    Credit::Block { block_bytes, solved }
}

/// Wraps credit_share: locks only for the credit step, then does the block
/// submit OUTSIDE the lock.
fn handle_submission(
    pool: &Arc<Mutex<Pool>>,
    worker: &str,
    height: u64,
    coinbase_nonce: [u8; 32],
    block_nonce: u32,
) -> serde_json::Value {
    let (credit, client, node) = {
        let mut p = pool.lock().unwrap();
        let c = credit_share(&mut p, worker, height, coinbase_nonce, block_nonce);
        (c, p.client.clone(), p.node.clone())
    };
    match credit {
        Credit::Reject(v) | Credit::Share(v) => v,
        Credit::Block { block_bytes, solved } => {
            let submit = submit_block_bytes(&client, &node, &block_bytes);
            json!({"ok":true,"kind":"block","solved_height":solved,"submit":submit})
        }
    }
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
        "/query/miner/pending" => {
            let p = pool.lock().unwrap();
            let cb = coinbase_with_extranonce(&p.tpl, &[0u8; 32]);
            let intro = intro_bytes(&p.tpl, &cb, 0);
            json!({
                "ret": 0,
                "height": p.tpl.height,
                "block_intro": hex::encode(intro),
                "target_hash": hex::encode(p.share_target),
                "coinbase_body": coinbase_body_hex(&cb),
                "mkrl_modify_list": [],
            })
            .to_string()
        }
        "/query/miner/notice" => {
            let want: u64 = params.get("height").and_then(|v| v.parse().ok()).unwrap_or(0);
            let wait: u64 = params
                .get("wait")
                .and_then(|v| v.parse().ok())
                .unwrap_or(45)
                .clamp(1, 300);
            let deadline = Instant::now() + Duration::from_secs(wait);
            loop {
                let h = pool.lock().unwrap().tpl.height; // brief lock only
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
            // Credit the announced payout address when the worker sends one.
            let worker = params
                .get("worker")
                .filter(|w| is_payout_address(w))
                .cloned()
                .unwrap_or_else(|| peer.to_string());
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
            let mut p = pool.lock().unwrap();
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
                let p = pool.lock().unwrap();
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
            let p = pool.lock().unwrap();
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
