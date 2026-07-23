//! Validate the off-node ASERT reimplementation against REAL chain history.
//!
//! For each of the last N blocks it recomputes the difficulty from that block's
//! own timestamp, its parent's difficulty and the anchor block's timestamp, then
//! compares against what the chain actually stored. A mismatch anywhere means
//! the pool would build blocks the node rejects, so this is the go/no-go check
//! before pointing a pool at mainnet.
//!
//! Usage: asert-check [node_base] [count] [chain]

use pool_spike::difficulty::{ChainParams, next_difficulty};
use pool_spike::{find_u64, get_json, http_client};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let node = a
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    let node = node.trim_end_matches('/').to_string();
    let count: u64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let chain = a.get(3).cloned().unwrap_or_else(|| "mainnet".to_string());
    let params = ChainParams::from_name(&chain);

    let client = http_client();
    let tip = find_u64(
        &get_json(&client, &format!("{node}/query/latest")),
        "height",
    )
    .expect("no chain tip");

    println!("== asert-check ==");
    println!("node   = {node}");
    println!("chain  = {chain} (ASERT anchor at height {})", params.asert_height);
    println!("tip    = {tip}");

    let anchor_time = find_u64(
        &get_json(
            &client,
            &format!("{node}/query/block/intro?height={}", params.asert_height),
        ),
        "timestamp",
    )
    .expect("anchor block timestamp (is the node synced past the anchor?)");
    println!("anchor ts = {anchor_time}\n");

    let first = tip.saturating_sub(count - 1).max(params.asert_height + 1);
    let mut ok = 0u64;
    let mut bad = 0u64;
    for h in first..=tip {
        let b = get_json(&client, &format!("{node}/query/block/intro?height={h}"));
        let (Some(ts), Some(stored)) = (find_u64(&b, "timestamp"), find_u64(&b, "difficulty"))
        else {
            println!("h={h}  (missing block data, skipped)");
            continue;
        };
        let pb = get_json(&client, &format!("{node}/query/block/intro?height={}", h - 1));
        let Some(prev_diff) = find_u64(&pb, "difficulty") else {
            println!("h={h}  (missing parent, skipped)");
            continue;
        };
        let (ours, _target) =
            next_difficulty(&params, h, ts, prev_diff as u32, anchor_time);
        if ours as u64 == stored {
            ok += 1;
            println!("h={h}  OK        difficulty={stored}");
        } else {
            bad += 1;
            println!("h={h}  MISMATCH  ours={ours}  chain={stored}");
        }
    }

    println!("\n{ok} matched, {bad} mismatched");
    if bad == 0 && ok > 0 {
        println!("PASS: the off-node ASERT reproduces real chain difficulty exactly.");
    } else {
        println!("FAIL: do NOT point a pool at this chain until this matches.");
    }
}
