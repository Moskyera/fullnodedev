//! Manual pool settlement: read the PPLNS share counts (from the pool server if
//! it answers, otherwise from the accounting file it left behind), split the
//! pool's SPENDABLE BALANCE proportionally among PAYABLE miners with
//! pool_core::split_payout, and pay them in one or more signed transactions
//! (chunked to the node's 200-action limit).
//!
//! Safety properties (real money):
//!   * DRY-RUN by default — prints the planned split and pays NOTHING unless you
//!     pass `--commit`.
//!   * Exclusive - takes the wallet's settlement lock for the whole run, so it
//!     can never pay out of a wallet a running pool-server is already settling.
//!     Both read the CONFIRMED balance, so without the lock each would see the
//!     full balance and pay the same PPLNS window.
//!   * Idempotent - records every submitted payout tx hash in the SAME pending
//!     ledger the pool server keeps (`<wallet>.state.json`) and REFUSES to pay
//!     again while any prior payout is not yet final, so a re-run / crash / cron
//!     overlap cannot double-pay.
//!   * Balance-derived - never pays more than (matured balance - reserve); no
//!     fixed "total" that could overspend.
//!   * Payable-first — the proportional split is computed over payable addresses
//!     only, so unpayable IP-fallback keys never dilute honest miners.
//!   * Chunked — at most 190 recipients per tx, so a large payout is never
//!     rejected by the node's TX_ACTIONS_MAX=200 limit.
//!   * Honest — reports the node's accept/reject for every tx and the real
//!     before/after balances; no "looks funded" guesswork.
//!
//! Usage: pool-payout <pool_base> <node> <chain> [wallet_file] [reserve_units] [dust_units] [--commit]
//!   chain = mainnet | testnet | testnet:<adjust_blocks>:<target_time>
//!   (required - a wrong difficulty rule means rejected blocks)

use basis::interface::*;
use field::*;
use protocol::action::HacToTrs;
use protocol::transaction::TransactionType2;
use sys::*;

use pool_spike::difficulty::ChainParams;
use pool_spike::pool_core::split_payout;
use pool_spike::{
    PayoutTxState, acquire_settle_lock, balance, balance_units, classify_payout_tx,
    distributable_units, find_u64, get_json, http_client, is_payout_address, load_immature_units,
    load_or_create_wallet, load_pending_payout_txs, load_pplns_counts, mine_and_submit_block,
    pool_state_path, post_hex, save_pending_payout_txs,
};

/// Recipients per settlement transaction — safely under TX_ACTIONS_MAX (200).
const PAYOUT_CHUNK: usize = 190;

/// The leading 16 characters of a tx hash, for readable log lines. Never slices
/// mid-character, so a corrupt ledger entry cannot panic a settlement run.
fn short(hash: &str) -> &str {
    hash.get(..16).unwrap_or(hash)
}

/// Read (and retire) the ledger older builds kept privately, next to the wallet.
/// Its contents move into the shared ledger the pool server also reads, so an
/// upgrade cannot forget a payout that is still in flight. The file is renamed
/// rather than deleted, so nothing is destroyed if the adoption goes wrong.
fn take_legacy_ledger(wallet_file: &str) -> Vec<String> {
    let path = format!("{wallet_file}.payout-pending.json");
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let hashes: Vec<String> = serde_json::from_str(&txt).unwrap_or_default();
    let _ = std::fs::rename(&path, format!("{path}.migrated"));
    hashes
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
             chain is required: `mainnet`, `testnet`, or \
             `testnet:<difficulty_adjust_blocks>:<each_block_target_time>`."
        );
        std::process::exit(2);
    };
    // A testnet node reads its difficulty window and block time from its OWN
    // config, so accept them spelled out instead of assuming a pair that would
    // make the confirming block below unmineable.
    let Some(params) = ChainParams::parse(&chain) else {
        eprintln!(
            "chain must be `mainnet`, `testnet`, or \
             `testnet:<difficulty_adjust_blocks>:<each_block_target_time>` (got `{chain}`)"
        );
        std::process::exit(2);
    };
    let is_testnet = chain != "mainnet";
    let wallet_file = pos
        .get(3)
        .cloned()
        .unwrap_or_else(|| "pool-wallet.key".to_string());
    let reserve_units: u64 = pos.get(4).and_then(|s| s.parse().ok()).unwrap_or(5); // 0.5 HAC
    let dust_units: u64 = pos.get(5).and_then(|s| s.parse().ok()).unwrap_or(1);

    let client = http_client();
    println!("== pool-payout ({}) ==", if commit { "COMMIT" } else { "DRY-RUN" });
    // Exclusive claim on this wallet's settlement, held for the whole run. A
    // running pool-server holds the same lock, so this can never become a second
    // settler paying the same PPLNS window out of the same confirmed balance.
    let _settle_lock = match acquire_settle_lock(&wallet_file) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "REFUSING to run: another pool-server or pool-payout already holds \
                 {wallet_file} ({e}).\n\
                 Only one process may settle a wallet - stop the pool server first."
            );
            std::process::exit(1);
        }
    };
    let pool_acc = load_or_create_wallet(&wallet_file);
    let pool_addr = pool_acc.readable().to_string();
    let bal = balance(&client, &node, &pool_addr);
    // A balance this tool cannot value is NOT a zero balance: settling on it
    // would sign transactions for a number the node never reported.
    let Some(bal_units) = balance_units(&bal) else {
        eprintln!(
            "REFUSING to pay: the node reported a balance this tool cannot value ({bal:?})."
        );
        std::process::exit(1);
    };
    println!("wallet  = {pool_addr}");
    println!("balance = {bal} ({bal_units} units of 0.1 HAC)");

    // Idempotency guard: never start a new payout while a prior one is not yet
    // final. This is the SAME ledger the pool server keeps, so the two paths can
    // never each believe they are the only one paying. It fails SAFE: a payout
    // that is only shallowly confirmed, or whose state we could not determine,
    // counts as still in flight.
    let state_file = pool_state_path(&wallet_file);
    let mut prior = load_pending_payout_txs(&state_file);
    // Older builds of this tool kept their OWN ledger, which the server never
    // read. Fold anything left there into the shared one before deciding, so an
    // upgrade cannot lose track of a payout that is still in flight.
    for h in take_legacy_ledger(&wallet_file) {
        if !prior.contains(&h) {
            println!("  adopting payout tx {} from the old private ledger", short(&h));
            prior.push(h);
        }
    }
    if !prior.is_empty() {
        let mut still: Vec<String> = Vec::new();
        for h in &prior {
            let j = get_json(&client, &format!("{node}/query/transaction?hash={h}"));
            match classify_payout_tx(&j) {
                PayoutTxState::Buried(_) => {}
                PayoutTxState::Gone => println!(
                    "  prior payout tx {} is unknown to the node (rejected or dropped)",
                    short(h)
                ),
                PayoutTxState::Confirming(d) => {
                    println!("  prior payout tx {} is only {d} block(s) deep", short(h));
                    still.push(h.clone());
                }
                PayoutTxState::Pending => still.push(h.clone()),
                PayoutTxState::Unknown => {
                    eprintln!("  cannot determine the state of payout tx {}", short(h));
                    still.push(h.clone());
                }
            }
        }
        if !still.is_empty() {
            if let Err(e) = save_pending_payout_txs(&state_file, &still) {
                eprintln!("could not update the pending ledger {state_file}: {e}");
            }
            eprintln!(
                "REFUSING to pay: {} prior payout tx(s) are not final yet:\n  {}\n\
                 Wait for them to be buried (or definitively dropped) before settling again.",
                still.len(),
                still.join("\n  ")
            );
            std::process::exit(1);
        }
        println!("prior payout(s) all final; clearing the ledger.");
        if let Err(e) = save_pending_payout_txs(&state_file, &[]) {
            eprintln!("REFUSING to pay: cannot clear the pending ledger {state_file}: {e}");
            std::process::exit(1);
        }
    }

    // 1) PPLNS counts. Try the live pool server first, then fall back to the
    // accounting file it left behind - holding the settlement lock means the
    // server is stopped, so /stats normally cannot answer at all.
    let stats = get_json(&client, &format!("{pool_base}/stats"));
    let rows = stats
        .get("workers")
        .and_then(|w| w.as_array())
        .cloned()
        .unwrap_or_default();
    let mut counts: Vec<(String, u64)> = rows
        .iter()
        .filter_map(|r| {
            let arr = r.as_array()?;
            Some((arr.first()?.as_str()?.to_string(), arr.get(1)?.as_u64()?))
        })
        .collect();
    if counts.is_empty() {
        counts = load_pplns_counts(&state_file);
        if !counts.is_empty() {
            println!("(pool server not answering; using the share window recorded in {state_file})");
        }
    }
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
    // Apply the SAME maturity gate as the automatic settlement: the pool server
    // records the coinbase of every block it found that is not yet buried, and
    // that income must not be paid out while a reorg could still take it back.
    let immature_units = load_immature_units(&state_file);
    if immature_units > 0 {
        println!("({immature_units} unit(s) of block income are not yet final and are held back)");
    }
    let Some(distributable) = distributable_units(bal_units, immature_units, reserve_units) else {
        println!(
            "matured balance ({} units) <= reserve {reserve_units} - nothing spendable",
            bal_units.saturating_sub(immature_units)
        );
        return;
    };
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
             (Tip: the pool server settles automatically on its timer, and holds this wallet's\n\
             settlement lock while it runs, so this tool only works while the server is stopped.)"
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
        // Record the hash in the shared pending ledger BEFORE submitting, so a
        // crash after submit still blocks a duplicate payout on the next run. If
        // that write fails, stop: an untracked payout is one a later run (or the
        // pool server) could pay all over again.
        let txhash = hex::encode(tx.hash().serialize());
        submitted.push(txhash.clone());
        if let Err(e) = save_pending_payout_txs(&state_file, &submitted) {
            eprintln!(
                "  cannot record the payout tx in {state_file} ({e}); ABORTING before submit so \
                 nothing is paid untracked."
            );
            std::process::exit(1);
        }

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
        if is_testnet && accepted {
            let (h, blkresp) = mine_and_submit_block(
                &client,
                &node,
                &pool_addr,
                vec![Box::new(tx) as Box<dyn Transaction>],
                &params,
            );
            println!("    confirming block {h} -> {blkresp}");
        }
    }

    if all_ok {
        println!("\nAll payout tx(s) accepted. They stay in the shared pending ledger until they");
        println!("are buried deep enough that a reorg cannot undo them; re-running before then is");
        println!("safe: this tool and the pool server both refuse to double-pay.");
    } else {
        eprintln!("\nSome payout tx(s) were rejected or failed to sign — see above. Ledger keeps the");
        eprintln!("submitted hashes so a retry will not double-pay the accepted ones.");
    }
    println!("pool wallet after = {}", balance(&client, &node, &pool_addr));
}
