//! Test miner for the pool protocol: pulls work, mines the 32-bit block nonce
//! against the pool's share target, submits shares. This is the worker side
//! that the real poworker will later speak.
//!
//! On `mkrl_modify_list`: this worker deliberately never rebuilds the merkle
//! root. `/work` hands it the FULL 89-byte header the pool itself reconstructs,
//! merkle root already folded, and the only bytes touched here are the nonce at
//! 79..83. That is what keeps it correct now that a pool block carries the
//! node's transactions - the sibling list only matters to a worker that computes
//! the root itself, which is the standard `/query/miner/pending` protocol, not
//! this one. Change that and this file must honour the list too.
//!
//! Usage: test-miner [pool_base] [worker_address] [shares_to_find]
//!   The worker id must be a payable HAC address: the pool credits shares under
//!   that key and refuses one it could never pay, exactly as on the paid path.

use pool_spike::pool_core;
use pool_spike::{find_str, find_u64, get_json, http_client};

/// Consecutive rejected submissions before this miner stops and says why.
/// Mining on regardless is how a worker burns hours producing nothing: the pool
/// rebuilds every share's header itself, so a worker hashing a different header
/// has EVERY share rejected, forever, however long it keeps trying.
const MAX_REJECTS: u64 = 8;

/// Did the pool credit this submission? The pool answers `{"ok":true,...}` for a
/// credited share or block and `{"ok":false,"kind":...}` for anything else, so
/// counting a submission as "found" without reading `ok` reports work that was
/// never credited - which is exactly what this miner used to print.
fn accepted(resp: &serde_json::Value) -> bool {
    resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false)
}

fn reject_kind(resp: &serde_json::Value) -> String {
    let kind = resp
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    match resp.get("err").and_then(|v| v.as_str()) {
        Some(err) => format!("{kind} ({err})"),
        None => kind.to_string(),
    }
}

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
    let mut rejected = 0u64;
    let mut streak = 0u64;
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
        // Nothing else in the header is touched, so the merkle root the pool
        // folded from its own coinbase and the node's transactions stays intact.
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
                // Only a submission the pool CREDITED counts. Counting every
                // submission made this miner report shares it was never paid for.
                if accepted(&r) {
                    found += 1;
                    streak = 0;
                    continue;
                }
                rejected += 1;
                streak += 1;
                if streak >= MAX_REJECTS {
                    eprintln!(
                        "the pool rejected {streak} submissions in a row (last: {}). It rebuilds \
                         every share's header itself, so this worker is hashing a DIFFERENT \
                         header than the pool and nothing it submits can ever be credited. \
                         Stopping rather than mining for nothing.",
                        reject_kind(&r)
                    );
                    break;
                }
            }
            None => println!("no share found in range at height {height}; refetching work"),
        }
    }
    println!("done: {found} share(s) credited to {worker}, {rejected} rejected");
    if found < want {
        // A non-zero exit so a script driving this miner cannot mistake a run
        // that was rejected wholesale for a successful one.
        std::process::exit(1);
    }
}
