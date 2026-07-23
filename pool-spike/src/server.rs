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

use pool_spike::pool_core::{self, Pplns};
use pool_spike::{
    Template, assemble_block, coinbase_with_extranonce, fetch_template, http_client, intro_bytes,
    submit_block_bytes,
};

use serde_json::json;

struct Pool {
    node: String,
    payout: String,
    client: reqwest::blocking::Client,
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
        self.tpl = fetch_template(&self.client, &self.node, &self.payout);
        self.network_target = pool_core::network_target_hash(self.tpl.difficulty);
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
    let payout = a
        .get(2)
        .cloned()
        .unwrap_or_else(|| "1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9".to_string());
    let listen = a.get(3).cloned().unwrap_or_else(|| "127.0.0.1:9777".to_string());
    let share_bits: u32 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);

    let client = http_client();
    let tpl = fetch_template(&client, &node, &payout);
    let network_target = pool_core::network_target_hash(tpl.difficulty);

    println!("== pool-server ==");
    println!("node    = {node}");
    println!("payout  = {payout}");
    println!("listen  = {listen}");
    println!("share   = {share_bits} leading zero bits");
    println!("height  = {} (template)", tpl.height);

    let pool = Arc::new(Mutex::new(Pool {
        node,
        payout,
        client,
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
    let body = route(&path, &params, &pool);
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

fn route(path: &str, params: &HashMap<String, String>, pool: &Arc<Mutex<Pool>>) -> String {
    match path {
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

            if height != p.tpl.height {
                return json!({"ok":false,"kind":"stale","height":p.tpl.height}).to_string();
            }
            let Some(en) = p.workers.get(&worker).copied() else {
                return json!({"ok":false,"kind":"invalid","err":"unknown worker"}).to_string();
            };

            let cb = coinbase_with_extranonce(&p.tpl, &en);
            let intro = intro_bytes(&p.tpl, &cb, nonce);
            if !pool_core::meets_target(p.tpl.height, &intro, &p.share_target) {
                return json!({"ok":false,"kind":"invalid","err":"above share target"}).to_string();
            }

            p.pplns.record(&worker);
            p.accepted += 1;
            let is_block = pool_core::meets_target(p.tpl.height, &intro, &p.network_target);
            if !is_block {
                return json!({"ok":true,"kind":"share","accepted":p.accepted}).to_string();
            }

            // Full network solution: assemble and submit the real block.
            let blk = assemble_block(&p.tpl, &cb, nonce);
            let submit = submit_block_bytes(&p.client, &p.node, &blk);
            p.blocks += 1;
            let solved = p.tpl.height;
            // Move to the next template once the node has committed it.
            for _ in 0..6 {
                std::thread::sleep(std::time::Duration::from_millis(300));
                p.refresh();
                if p.tpl.height > solved {
                    break;
                }
            }
            json!({
                "ok": true, "kind": "block", "solved_height": solved,
                "submit": submit, "next_height": p.tpl.height, "blocks": p.blocks
            })
            .to_string()
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
