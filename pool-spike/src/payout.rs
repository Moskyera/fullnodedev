//! Closes the pool loop: read the live PPLNS share counts from the pool server,
//! split the pool's earnings proportionally with pool_core::split_payout, and
//! pay every miner in ONE signed transaction (the proven batched settlement).
//!
//! Accounting unit here is 0.1 HAC (Amount unit 247), so `units` map directly to
//! the "mantissa:247" amount strings.
//!
//! Usage: pool-payout [pool_base] [node_base] [total_units] [fee_units] [dust_units]

use basis::interface::*;
use field::*;
use protocol::action::HacToTrs;
use protocol::transaction::TransactionType2;
use sys::*;

use pool_spike::pool_core::split_payout;
use pool_spike::{
    balance, get_json, http_client, is_payout_address, load_or_create_wallet,
    mine_and_submit_block, post_hex,
};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let pool_base = a
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:9777".to_string());
    let node = a
        .get(2)
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:8088".to_string());
    let pool_base = pool_base.trim_end_matches('/').to_string();
    let node = node.trim_end_matches('/').to_string();
    let total_units: u64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(50); // 5.0 HAC
    let fee_units: u64 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(5); // 0.5 HAC pool fee
    let dust_units: u64 = a.get(5).and_then(|s| s.parse().ok()).unwrap_or(1);
    let wallet_file = a
        .get(6)
        .cloned()
        .unwrap_or_else(|| "pool-wallet.key".to_string());

    let client = http_client();
    println!("== pool-payout ==");
    // Same wallet file the pool server mines to; its key signs the payout tx.
    let pool_acc = load_or_create_wallet(&wallet_file);
    println!("balance     = {}", balance(&client, &node, pool_acc.readable()));

    // 1) live PPLNS counts from the pool server
    let stats = get_json(&client, &format!("{pool_base}/stats"));
    let rows = stats
        .get("workers")
        .and_then(|w| w.as_array())
        .cloned()
        .unwrap_or_default();
    let counts: Vec<(String, u64)> = rows
        .iter()
        .filter_map(|r| {
            let arr = r.as_array()?;
            Some((arr.first()?.as_str()?.to_string(), arr.get(1)?.as_u64()?))
        })
        .collect();
    if counts.is_empty() {
        println!("no shares recorded yet — nothing to pay");
        return;
    }
    println!("\nPPLNS shares: {counts:?}");

    // 2) exact proportional split (largest remainder, fee off the top, dust dropped)
    let split = split_payout(total_units, fee_units, dust_units, &counts);
    println!(
        "split of {total_units} units (fee {fee_units}, dust {dust_units}) -> {split:?}   [1 unit = 0.1 HAC]"
    );

    // 3) one signed transaction paying everyone
    let main = Address::from(*pool_acc.address());
    let fee = Amount::from("1:246").expect("tx fee"); // 0.01 HAC
    let mut tx = TransactionType2::new_by(main, fee, curtimes());
    // PPLNS keys ARE payout addresses when the worker announced one via
    // &worker=<address>; anything else (an IP fallback) cannot be auto-paid.
    let mut paid: Vec<(String, u64)> = Vec::new();
    for (worker, units) in &split {
        if !is_payout_address(worker) {
            println!("  (skip {worker}: no payout address announced by that worker)");
            continue;
        }
        let to = Address::from_readable(worker).expect("payout address");
        let amt = Amount::from(&format!("{units}:247")).expect("amount");
        let mut act = HacToTrs::new();
        act.to = AddrOrPtr::from_addr(to);
        act.hacash = amt;
        tx.push_action(Box::new(act)).expect("push action");
        println!("  -> {worker} = {units}:247");
        paid.push((worker.clone(), *units));
    }
    if paid.is_empty() {
        println!("nothing payable");
        return;
    }

    println!("\nbefore:");
    for (addr, _) in &paid {
        println!("  {addr} = {}", balance(&client, &node, addr));
    }

    tx.fill_sign(&pool_acc).expect("fill_sign");
    let body_hex = hex::encode(tx.serialize());
    let resp = post_hex(
        &client,
        &format!("{node}/submit/transaction?hexbody=true"),
        &body_hex,
    );
    println!("\n/submit/transaction -> {resp}");

    // 4) confirm by mining a block that includes the payout tx
    let (h, blkresp) = mine_and_submit_block(
        &client,
        &node,
        pool_acc.readable(),
        vec![Box::new(tx) as Box<dyn Transaction>],
    );
    println!("mined confirming block {h} -> {blkresp}");

    for _ in 0..12 {
        std::thread::sleep(std::time::Duration::from_millis(700));
        if paid
            .iter()
            .all(|(addr, _)| !balance(&client, &node, addr).starts_with("0:"))
        {
            break;
        }
    }
    println!("\nafter:");
    for (addr, units) in &paid {
        println!(
            "  {addr} = {}   (paid {units}:247)",
            balance(&client, &node, addr)
        );
    }
    println!("  pool wallet = {}", balance(&client, &node, pool_acc.readable()));
    println!("\nLOOP CLOSED: shares -> PPLNS -> proportional split -> one signed payout tx -> on-chain.");
}
