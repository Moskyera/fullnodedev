//! Manual pool settlement: read the live PPLNS share counts from the pool
//! server, split the pool's SPENDABLE BALANCE proportionally among PAYABLE
//! miners with pool_core::split_payout, and pay them in one or more signed
//! transactions (chunked to the node's 200-action limit).
//!
//! Safety properties (real money):
//!   * DRY-RUN by default — prints the planned split and pays NOTHING unless you
//!     pass `--commit`.
//!   * Idempotent — records every submitted payout tx hash in a pending ledger
//!     and REFUSES to pay again while any prior payout is still in the mempool,
//!     so a re-run / crash / cron overlap cannot double-pay.
//!   * Balance-derived — never pays more than (balance - reserve); no fixed
//!     "total" that could overspend.
//!   * Payable-first — the proportional split is computed over payable addresses
//!     only, so unpayable IP-fallback keys never dilute honest miners.
//!   * Chunked — at most 190 recipients per tx, so a large payout is never
//!     rejected by the node's TX_ACTIONS_MAX=200 limit.
//!   * Honest — reports the node's accept/reject for every tx and the real
//!     before/after balances; no "looks funded" guesswork.
//!
//! Usage: pool-payout <pool_base> <node> <chain> [wallet_file] [reserve_units] [dust_units] [--commit]
//!   chain = mainnet | testnet (required — wrong difficulty => rejected blocks)

use basis::interface::*;
use field::*;
use protocol::action::HacToTrs;
use protocol::transaction::TransactionType2;
use sys::*;

use pool_spike::pool_core::split_payout;
use pool_spike::{
    balance, balance_units, find_u64, get_json, http_client, is_payout_address,
    load_or_create_wallet, mine_and_submit_block, post_hex,
};

/// Recipients per settlement transaction — safely under TX_ACTIONS_MAX (200).
const PAYOUT_CHUNK: usize = 190;

fn ledger_path(wallet: &str) -> String {
    format!("{wallet}.payout-pending.json")
}

fn load_ledger(path: &str) -> Vec<String> {
    let Ok(txt) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(&txt).unwrap_or_default()
}

fn save_ledger(path: &str, hashes: &[String]) {
    let body = serde_json::to_string(hashes).unwrap_or_else(|_| "[]".to_string());
    let tmp = format!("{path}.tmp.{}", std::process::id());
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Is `hash` still an unconfirmed tx in the node's mempool?
fn tx_still_pending(client: &reqwest::blocking::Client, node: &str, hash: &str) -> bool {
    let j = get_json(client, &format!("{node}/query/transaction?hash={hash}"));
    let ret_ok = find_u64(&j, "ret") == Some(0);
    let is_pending = j
        .get("data")
        .and_then(|d| d.get("pending"))
        .and_then(|v| v.as_bool())
        .or_else(|| j.get("pending").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    ret_ok && is_pending
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let commit = a.iter().any(|x| x == "--commit");
    let pos: Vec<String> = a
        .iter()
        .skip(1)
        .filter(|x| !x.starts_with("--"))
        .cloned()
        .collect();

    let pool_base = pos
        .first()
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:9777".to_string());
    let node = pos
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:8088".to_string());
    let pool_base = pool_base.trim_end_matches('/').to_string();
    let node = node.trim_end_matches('/').to_string();
    let Some(chain) = pos.get(2).cloned() else {
        eprintln!(
            "usage: pool-payout <pool_base> <node> <chain> [wallet_file] [reserve_units] [dust_units] [--commit]\n\
             chain is required and must be `mainnet` or `testnet`."
        );
        std::process::exit(2);
    };
    if chain != "mainnet" && chain != "testnet" {
        eprintln!("chain must be `mainnet` or `testnet` (got `{chain}`)");
        std::process::exit(2);
    }
    let wallet_file = pos
        .get(3)
        .cloned()
        .unwrap_or_else(|| "pool-wallet.key".to_string());
    let reserve_units: u64 = pos.get(4).and_then(|s| s.parse().ok()).unwrap_or(5); // 0.5 HAC
    let dust_units: u64 = pos.get(5).and_then(|s| s.parse().ok()).unwrap_or(1);

    let client = http_client();
    println!("== pool-payout ({}) ==", if commit { "COMMIT" } else { "DRY-RUN" });
    let pool_acc = load_or_create_wallet(&wallet_file);
    let pool_addr = pool_acc.readable().to_string();
    let bal = balance(&client, &node, &pool_addr);
    let bal_units = balance_units(&bal);
    println!("wallet  = {pool_addr}");
    println!("balance = {bal} ({bal_units} units of 0.1 HAC)");

    // Idempotency guard: never start a new payout while a prior one is still in
    // the mempool. Resolve the ledger first.
    let ledger = ledger_path(&wallet_file);
    let prior = load_ledger(&ledger);
    if !prior.is_empty() {
        let still: Vec<String> = prior
            .iter()
            .filter(|h| tx_still_pending(&client, &node, h))
            .cloned()
            .collect();
        if !still.is_empty() {
            save_ledger(&ledger, &still);
            eprintln!(
                "REFUSING to pay: {} prior payout tx(s) still pending in the mempool:\n  {}\n\
                 Wait for them to confirm (or drop) before settling again.",
                still.len(),
                still.join("\n  ")
            );
            std::process::exit(1);
        }
        println!("prior payout(s) all resolved; clearing ledger.");
        save_ledger(&ledger, &[]);
    }

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

    // 2) payable-only, balance-derived, exact proportional split
    let payable_counts: Vec<(String, u64)> = counts
        .iter()
        .filter(|(w, _)| is_payout_address(w))
        .cloned()
        .collect();
    let skipped = counts.len() - payable_counts.len();
    if skipped > 0 {
        println!("({skipped} worker(s) without an announced payout address are excluded)");
    }
    if payable_counts.is_empty() {
        println!("no payable workers (nobody announced a payout address) — nothing to pay");
        return;
    }
    if bal_units <= reserve_units + 1 {
        println!("balance {bal_units} units <= reserve {reserve_units} — nothing spendable");
        return;
    }
    let distributable = bal_units - reserve_units;
    let split = split_payout(distributable, 0, dust_units, &payable_counts);
    if split.is_empty() {
        println!("split produced no payable rows (all below dust {dust_units}) — nothing to pay");
        return;
    }
    let n_tx = split.len().div_ceil(PAYOUT_CHUNK);
    println!(
        "\nplanned split of {distributable} units over {} payable miner(s) in {n_tx} tx(s):",
        split.len()
    );
    for (w, u) in &split {
        println!("  -> {w} = {u}:247");
    }

    if !commit {
        println!(
            "\nDRY-RUN: nothing was submitted. Re-run with --commit to pay.\n\
             (Tip: the pool server settles automatically on its timer; use this tool only if you\n\
             run the server with automatic settlement disabled, and never both at once.)"
        );
        return;
    }

    // 3) submit one or more chunked, signed transactions.
    let main = Address::from(*pool_acc.address());
    let mut submitted: Vec<String> = Vec::new();
    let mut all_ok = true;
    for chunk in split.chunks(PAYOUT_CHUNK) {
        let fee = Amount::from("1:246").expect("tx fee"); // 0.01 HAC (from reserve)
        let mut tx = TransactionType2::new_by(main.clone(), fee, curtimes());
        let mut pushed = 0usize;
        for (worker, units) in chunk {
            let Ok(to) = Address::from_readable(worker) else {
                continue;
            };
            let Ok(amt) = Amount::from(&format!("{units}:247")) else {
                eprintln!("  skip {worker}: amount {units}:247 not representable");
                continue;
            };
            let mut act = HacToTrs::new();
            act.to = AddrOrPtr::from_addr(to);
            act.hacash = amt;
            if tx.push_action(Box::new(act)).is_err() {
                break;
            }
            pushed += 1;
        }
        if pushed == 0 {
            continue;
        }
        if tx.fill_sign(&pool_acc).is_err() {
            eprintln!("  signing failed for a chunk; skipping");
            all_ok = false;
            continue;
        }
        // Record the hash in the pending ledger BEFORE submitting, so a crash
        // after submit still blocks a duplicate payout on the next run.
        let txhash = hex::encode(tx.hash().serialize());
        submitted.push(txhash.clone());
        save_ledger(&ledger, &submitted);

        let body_hex = hex::encode(tx.serialize());
        let resp = post_hex(
            &client,
            &format!("{node}/submit/transaction?hexbody=true"),
            &body_hex,
        );
        let accepted = serde_json::from_str::<serde_json::Value>(&resp)
            .ok()
            .and_then(|v| find_u64(&v, "ret"))
            == Some(0);
        all_ok &= accepted;
        println!(
            "  tx {} paying {pushed} miner(s): {}",
            &txhash[..txhash.len().min(16)],
            if accepted { "ACCEPTED".to_string() } else { format!("REJECTED -> {resp}") }
        );

        // On testnet the pool has no other miners, so self-mine a confirming
        // block that includes this tx. On mainnet the tx waits in the mempool for
        // the network to include it (the pool mines coinbase-only blocks).
        if chain == "testnet" && accepted {
            let (h, blkresp) = mine_and_submit_block(
                &client,
                &node,
                &pool_addr,
                vec![Box::new(tx) as Box<dyn Transaction>],
                &pool_spike::difficulty::ChainParams::from_name(&chain),
            );
            println!("    confirming block {h} -> {blkresp}");
        }
    }

    if all_ok {
        println!("\nAll payout tx(s) accepted. They remain in the pending ledger until confirmed;");
        println!("re-running before then is safe — it will refuse to double-pay.");
    } else {
        eprintln!("\nSome payout tx(s) were rejected or failed to sign — see above. Ledger keeps the");
        eprintln!("submitted hashes so a retry will not double-pay the accepted ones.");
    }
    println!("pool wallet after = {}", balance(&client, &node, &pool_addr));
}
