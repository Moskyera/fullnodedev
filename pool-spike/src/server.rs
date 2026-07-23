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
use std::io::{BufRead, BufReader, Write};
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
    find_str, get_json, http_client, intro_bytes, is_payout_address, load_or_create_wallet,
    post_hex, submit_block_bytes,
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
}

impl Pool {
    fn refresh(&mut self) {
        let before = self.tpl.height;
        self.tpl = fetch_template(&self.client, &self.node, &self.payout, &self.params);
        // Use the template's EXACT target, not u32_to_hash(difficulty).
        self.network_target = self.tpl.target;
        if self.tpl.height != before {
            // A share is only valid against the template it was mined for.
            self.seen.clear();
        }
        self.confirm_submitted();
    }

    /// Count a submitted block only once the chain still holds OUR hash at that
    /// height. Anything else lost a reorg and must not be paid for.
    fn confirm_submitted(&mut self) {
        let tip = self.tpl.height.saturating_sub(1);
        let mut pending = Vec::new();
        for (h, ours) in std::mem::take(&mut self.submitted) {
            if h > tip {
                pending.push((h, ours));
                continue;
            }
            let j = get_json(
                &self.client,
                &format!("{}/query/block/intro?height={h}", self.node),
            );
            match find_str(&j, "hash") {
                Some(chain_hash) => {
                    if chain_hash == hex::encode(ours) {
                        self.blocks += 1;
                    } else {
                        self.orphaned += 1;
                        println!("[reorg] our block {h} was orphaned (chain holds {chain_hash})");
                    }
                }
                None => pending.push((h, ours)), // node has not stored it yet
            }
        }
        self.submitted = pending;
    }

    /// Stable per-worker extranonce -> private search space (coinbase miner_nonce).
    fn extranonce_for(&mut self, worker: &str) -> [u8; 32] {
        if let Some(en) = self.workers.get(worker) {
            return *en;
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

/// A "mantissa:unit" balance expressed in units of 0.1 HAC (unit 247).
fn balance_units(bal: &str) -> u64 {
    let Some((m, u)) = bal.split_once(':') else {
        return 0;
    };
    let (Ok(m), Ok(u)) = (m.trim().parse::<u64>(), u.trim().parse::<i64>()) else {
        return 0;
    };
    if u < 247 {
        return 0; // below our accounting granularity
    }
    let exp = (u - 247) as u32;
    if exp > 18 {
        return u64::MAX;
    }
    m.saturating_mul(10u64.pow(exp))
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
    let tpl = fetch_template(&client, &node, &payout, &params);
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
    };
    pool.load_state();
    let pool = Arc::new(Mutex::new(pool));

    // Automatic settlement on a timer.
    {
        let p = pool.clone();
        let wf = wallet_file.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(settle_secs));
                settle_once(&p, &wf);
            }
        });
    }

    let listener = TcpListener::bind(&listen).expect("bind");
    println!("listening...\n");
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let p = pool.clone();
                std::thread::spawn(move || handle(s, p));
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
}

/// Pay every miner their PPLNS share of the pool's spendable balance, in ONE
/// signed transaction submitted to the node's mempool.
fn settle_once(pool: &Arc<Mutex<Pool>>, wallet_file: &str) {
    let (node, counts) = {
        let p = pool.lock().unwrap();
        (p.node.clone(), p.pplns.counts())
    };
    if counts.is_empty() {
        return;
    }
    let acc = load_or_create_wallet(wallet_file);
    let client = http_client();
    let bal = balance(&client, &node, acc.readable());
    let units = balance_units(&bal);
    // Keep a reserve so the wallet can always pay tx fees.
    let reserve = 5u64; // 0.5 HAC
    if units <= reserve + 1 {
        return;
    }
    let distributable = units - reserve;
    let fee_units = (distributable / 10).max(1); // 10% pool fee
    let split = split_payout(distributable, fee_units, 1, &counts);
    let payable: Vec<(String, u64)> = split
        .into_iter()
        .filter(|(w, _)| is_payout_address(w))
        .collect();
    if payable.is_empty() {
        return;
    }

    let main = Address::from(*acc.address());
    let fee = Amount::from("1:246").expect("fee");
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
    let body = hex::encode(tx.serialize());
    let resp = post_hex(
        &client,
        &format!("{node}/submit/transaction?hexbody=true"),
        &body,
    );
    println!(
        "[settle] paid {} miner(s) from {} units -> {resp}",
        payable.len(),
        distributable
    );
}

/// Validate one submitted solution, record it, and on a network-target hit
/// assemble + submit the real block. Shared by both protocols.
fn accept_share(
    p: &mut Pool,
    worker: &str,
    height: u64,
    coinbase_nonce: [u8; 32],
    block_nonce: u32,
) -> serde_json::Value {
    if height != p.tpl.height {
        return json!({"ok":false,"kind":"stale","height":p.tpl.height});
    }
    // Reject replays BEFORE any crediting: the same solution must never be
    // counted twice, or a miner could inflate its share of the payout.
    let key = (height, coinbase_nonce, block_nonce);
    if p.seen.contains(&key) {
        return json!({"ok":false,"kind":"duplicate"});
    }
    // Rebuild exactly what the worker hashed.
    let cb = coinbase_with_extranonce(&p.tpl, &coinbase_nonce);
    let intro = intro_bytes(&p.tpl, &cb, block_nonce);
    let hash = pool_core::hash_of(p.tpl.height, &intro);
    if !pool_core::beats(&hash, &p.share_target) {
        return json!({"ok":false,"kind":"invalid","err":"above share target"});
    }

    p.seen.insert(key);
    p.pplns.record(worker);
    p.accepted += 1;
    p.save_state();

    if !pool_core::beats(&hash, &p.network_target) {
        return json!({"ok":true,"kind":"share","accepted":p.accepted});
    }

    let blk = assemble_block(&p.tpl, &cb, block_nonce);
    let submit = submit_block_bytes(&p.client, &p.node, &blk);
    let solved = p.tpl.height;
    // Counted only after confirm_submitted() sees it stick.
    p.submitted.push((solved, hash));
    for _ in 0..6 {
        std::thread::sleep(Duration::from_millis(300));
        p.refresh();
        if p.tpl.height > solved {
            break;
        }
    }
    p.save_state();
    json!({"ok":true,"kind":"block","solved_height":solved,"submit":submit,
           "next_height":p.tpl.height,"blocks":p.blocks})
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
    let Ok(peek) = s.try_clone() else { return };
    let mut reader = BufReader::new(peek);
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
            let mut p = pool.lock().unwrap();
            let r = accept_share(&mut p, &worker, height, cn, block_nonce);
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
            let mut p = pool.lock().unwrap();
            let Some(en) = p.workers.get(&worker).copied() else {
                return json!({"ok":false,"kind":"invalid","err":"unknown worker"}).to_string();
            };
            accept_share(&mut p, &worker, height, en, nonce).to_string()
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
