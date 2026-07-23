//! Test miner for the pool protocol: pulls work, mines the 32-bit block nonce
//! against the pool's share target, submits shares. This is the worker side
//! that the real poworker will later speak.
//!
//! Usage: test-miner [pool_base] [worker_name] [shares_to_find]

use pool_spike::pool_core;
use pool_spike::{find_str, find_u64, get_json, http_client};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let pool = a
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:9777".to_string());
    let pool = pool.trim_end_matches('/').to_string();
    let worker = a.get(2).cloned().unwrap_or_else(|| "w1".to_string());
    let want: u64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(3);

    let client = http_client();
    println!("== test-miner {worker} -> {pool} (want {want} shares) ==");

    let mut found = 0u64;
    while found < want {
        let w = get_json(&client, &format!("{pool}/work?worker={worker}"));
        let (Some(height), Some(intro_hex), Some(st_hex)) = (
            find_u64(&w, "height"),
            find_str(&w, "intro"),
            find_str(&w, "share_target"),
        ) else {
            println!("bad work response: {w}");
            break;
        };

        let mut intro = hex::decode(&intro_hex).expect("intro hex");
        if intro.len() != 89 {
            println!("unexpected header length {}", intro.len());
            break;
        }
        let stv = hex::decode(&st_hex).expect("share target hex");
        let mut share_target = [0u8; 32];
        share_target.copy_from_slice(&stv);

        // Mine: the block nonce lives at header bytes 79..83 (big-endian u32).
        let mut hit = None;
        for nonce in 0u32..3_000_000 {
            intro[79..83].copy_from_slice(&nonce.to_be_bytes());
            if pool_core::meets_target(height, &intro, &share_target) {
                hit = Some(nonce);
                break;
            }
        }

        match hit {
            Some(nonce) => {
                let r = get_json(
                    &client,
                    &format!("{pool}/share?worker={worker}&height={height}&nonce={nonce}"),
                );
                println!("height={height} nonce={nonce} -> {r}");
                found += 1;
            }
            None => println!("no share found in range at height {height}; refetching work"),
        }
    }
    println!("done: {found} share(s) submitted by {worker}");
}
