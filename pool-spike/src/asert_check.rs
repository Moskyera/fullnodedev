//! Validate the off-node ASERT reimplementation against REAL chain history.
//!
//! For each of the last N blocks it recomputes the difficulty from that block's
//! own timestamp, its parent's difficulty and the anchor block's timestamp, then
//! compares against what the chain actually stored. A mismatch anywhere means
//! the pool would build blocks the node rejects, so this is the go/no-go check
//! before pointing a pool at mainnet.
//!
//! It checks BOTH quantities the node validates, because they are not
//! interchangeable: the header `difficulty` num, and the exact 32-byte PoW
//! target (each historical block was accepted by the network, so its own hash
//! must satisfy the target we recompute). A num-only check would pass a build
//! whose target hash is too tight, and such a pool silently discards solutions
//! the node would have accepted - lost blocks, lost revenue.
//!
//! Usage: asert-check [node_base] [count] [chain]
//!   chain = mainnet | testnet | testnet:<adjust_blocks>:<target_time>

use basis::difficulty::hash_bigger_than;
use pool_spike::difficulty::{ChainParams, next_difficulty};
use pool_spike::{find_str, find_u64, get_json, http_client};

/// The block's own PoW hash from a `/query/block/intro` response.
fn block_hash32(b: &serde_json::Value) -> Option<[u8; 32]> {
    let v = hex::decode(find_str(b, "hash")?).ok()?;
    (v.len() == 32).then(|| {
        let mut out = [0u8; 32];
        out.copy_from_slice(&v);
        out
    })
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let node = a
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    let node = node.trim_end_matches('/').to_string();
    // Clamp to >=1: `count - 1` below would otherwise underflow at count==0.
    let count: u64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(10).max(1);
    let chain = a.get(3).cloned().unwrap_or_else(|| "mainnet".to_string());
    let Some(params) = ChainParams::parse(&chain) else {
        eprintln!(
            "chain must be `mainnet`, `testnet`, or \
             `testnet:<difficulty_adjust_blocks>:<each_block_target_time>` (got `{chain}`)"
        );
        std::process::exit(2);
    };

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
        let (ours, target) = next_difficulty(&params, h, ts, prev_diff as u32, anchor_time);
        if ours as u64 != stored {
            bad += 1;
            println!("h={h}  MISMATCH  ours={ours}  chain={stored}");
            continue;
        }
        // The node validates a block against BOTH quantities: the header `num`
        // above AND the exact 32-byte PoW target, which on the from_big path is
        // more precise than u32_to_hash(num). Comparing only the num would pass a
        // build whose target hash is wrong, and the pool would then throw away
        // solutions the node accepts. This block WAS accepted by the network, so
        // its own hash must satisfy the target we just recomputed.
        match block_hash32(&b) {
            Some(bh) if hash_bigger_than(&bh, &target) => {
                bad += 1;
                println!(
                    "h={h}  TARGET-TOO-TIGHT  the chain's own block hash exceeds our target {}",
                    hex::encode(target)
                );
            }
            Some(_) => {
                ok += 1;
                println!("h={h}  OK        difficulty={stored}  target={}", hex::encode(target));
            }
            None => {
                // Without the block's hash only half the check ran; do not report
                // that as a pass.
                bad += 1;
                println!("h={h}  NO-HASH   could not read the block's own hash to verify the target");
            }
        }
    }

    println!("\n{ok} matched, {bad} mismatched");
    if bad == 0 && ok > 0 {
        println!("PASS: the off-node ASERT reproduces real chain difficulty exactly.");
    } else {
        println!("FAIL: do NOT point a pool at this chain until this matches.");
    }
}
