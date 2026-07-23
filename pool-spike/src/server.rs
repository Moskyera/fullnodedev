//! Minimal Hacash pool server (spike): serves work, validates shares with
//! pool_core, keeps PPLNS accounting, and submits full blocks to the node.
//! Blocking HTTP on std::net — no async runtime, no node changes.
//!
//! Endpoints:
//!   GET /work?worker=NAME   -> {height, intro, share_target, network_target, extranonce}
//!   GET /share?worker=NAME&height=H&nonce=N -> {ok, kind: share|block|stale|invalid}
//!   GET /stats              -> {height, accepted_shares, blocks, workers}
//!
//! Usage: pool-server [node_base] [pool_payout_addr] [listen_addr] [share_bits]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use pool_spike::difficulty::ChainParams;
use pool_spike::pool_core::{self, Pplns};
use pool_spike::{
    Template, assemble_block, coinbase_body_hex, coinbase_with_extranonce, fetch_template,
    http_client, intro_bytes, is_payout_address, load_or_create_wallet, submit_block_bytes,
};

use serde_json::json;

struct Pool {
    node: String,
    payout: String,
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
}

impl Pool {
    /// Re-read the tip and rebuild the template (after a block, or when stale).
    fn refresh(&mut self) {
        self.tpl = fetch_template(&self.client, &self.node, &self.payout, &self.params);
        // Use the template's EXACT target, not u32_to_hash(difficulty).
        self.network_target = self.tpl.target;
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
}

/// A target requiring `bits` leading zero bits (pool difficulty knob).
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
    let listen = a.get(3).cloned().unwrap_or_else(|| "127.0.0.1:9777".to_string());
    let share_bits: u32 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);
    let chain = a.get(5).cloned().unwrap_or_else(|| "testnet".to_string());
    let params = ChainParams::from_name(&chain);

    println!("== pool-server ==");
    println!("node    = {node}");
    // The pool's coinbase + settlement wallet. Key stays in the file.
    let wallet = load_or_create_wallet(&wallet_file);
    let payout = wallet.readable().to_string();

    let client = http_client();
    let tpl = fetch_template(&client, &node, &payout, &params);
    let network_target = tpl.target;

    println!("listen  = {listen}");
    println!("chain   = {chain} (ASERT at height {})", params.asert_height);
    println!("share   = {share_bits} leading zero bits");
    println!(
        "height  = {} (template, difficulty {})",
        tpl.height, tpl.difficulty
    );

    let pool = Arc::new(Mutex::new(Pool {
        node,
        payout,
        client,
        params,
        tpl,
        share_target: target_leading_zero_bits(share_bits),
        network_target,
        workers: HashMap::new(),
        next_en: 0,
        pplns: Pplns::new(1024),
        accepted: 0,
        blocks: 0,
    }));

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

/// Validate one submitted solution, record it, and on a network-target hit
/// assemble + submit the real block. Shared by our own protocol and the
/// standard miner API.
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
    // Rebuild exactly what the worker hashed: coinbase carrying ITS miner_nonce,
    // merkle root = coinbase hash (coinbase-only block), intro with its nonce.
    let cb = coinbase_with_extranonce(&p.tpl, &coinbase_nonce);
    let intro = intro_bytes(&p.tpl, &cb, block_nonce);
    if !pool_core::meets_target(p.tpl.height, &intro, &p.share_target) {
        return json!({"ok":false,"kind":"invalid","err":"above share target"});
    }
    p.pplns.record(worker);
    p.accepted += 1;
    if !pool_core::meets_target(p.tpl.height, &intro, &p.network_target) {
        return json!({"ok":true,"kind":"share","accepted":p.accepted});
    }
    let blk = assemble_block(&p.tpl, &cb, block_nonce);
    let submit = submit_block_bytes(&p.client, &p.node, &blk);
    p.blocks += 1;
    let solved = p.tpl.height;
    for _ in 0..6 {
        std::thread::sleep(std::time::Duration::from_millis(300));
        p.refresh();
        if p.tpl.height > solved {
            break;
        }
    }
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

fn route(
    path: &str,
    params: &HashMap<String, String>,
    pool: &Arc<Mutex<Pool>>,
    peer: &str,
) -> String {
    match path {
        // ---- standard Hacash miner API: an UNMODIFIED poworker can mine here.
        // The only difference from a real node is that target_hash carries the
        // POOL's share target, so the worker submits shares, not just blocks.
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
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait);
            loop {
                // brief lock only — never hold it while sleeping
                let h = pool.lock().unwrap().tpl.height;
                if h > want || std::time::Instant::now() >= deadline {
                    return json!({"ret":0,"height":h}).to_string();
                }
                std::thread::sleep(std::time::Duration::from_millis(400));
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
            // Credit the announced payout address when the worker sends one
            // (&worker=<addr>); otherwise fall back to the source IP, which the
            // operator would have to map by hand at payout time.
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
                println!("[{peer}] {kind} at height {height}");
                json!({"ret":0,"kind":kind}).to_string()
            } else {
                json!({"ret":1,"kind":kind,"err":r.get("err")}).to_string()
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
                "accepted_shares": p.accepted,
                "blocks": p.blocks,
                "share_window": p.pplns.total(),
                "workers": p.pplns.counts(),
            })
            .to_string()
        }
        _ => json!({"ok":false,"err":"no such endpoint"}).to_string(),
    }
}
