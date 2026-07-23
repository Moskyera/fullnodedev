//! P1.0 feasibility spike: assemble a coinbase-only block OFF-NODE whose
//! coinbase pays a CHOSEN address, CPU-mine it, and submit via /submit/block —
//! proving the node accepts an externally-chosen coinbase with no node change.
//! Run against a fresh local testnet (chain_id != 0).
//!
//! Usage:  pool-spike [node_base_url] [payout_privakey_address]

use std::env;

use pool_spike::difficulty::ChainParams;
use pool_spike::{balance, http_client, mine_and_submit_block};

fn main() {
    let args: Vec<String> = env::args().collect();
    let base = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:8088".to_string());
    let base = base.trim_end_matches('/').to_string();
    let payout = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9".to_string());

    println!("== pool-spike ==");
    println!("node   = {base}");
    println!("payout = {payout}");

    let params = ChainParams::from_name(&args.get(3).cloned().unwrap_or_else(|| "testnet".into()));
    let client = http_client();
    let (h, resp) = mine_and_submit_block(&client, &base, &payout, vec![], &params);
    println!("mined + submitted block {h} -> {resp}");

    std::thread::sleep(std::time::Duration::from_millis(900));
    println!("payout balance = {}", balance(&client, &base, &payout));
}
