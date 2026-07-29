//! Manual pool settlement: read the PPLNS share counts (from the pool server if
//! it answers, otherwise from the accounting file it left behind), split the
//! pool's SPENDABLE BALANCE proportionally among PAYABLE miners with
//! pool_core::split_payout, and pay them in one or more signed transactions
//! (chunked to the node's 200-action limit).
//!
//! Safety properties (real money):
//!   * DRY-RUN by default - prints the planned split and pays NOTHING unless you
//!     pass `--commit`.
//!   * Exclusive - takes the wallet's settlement lock for the whole run, so it
//!     can never pay out of a wallet a running hbit-pool-server is already settling.
//!     Both read the CONFIRMED balance, so without the lock each would see the
//!     full balance and pay the same PPLNS window.
//!   * Idempotent - records every submitted payout tx hash in the SAME pending
//!     ledger the pool server keeps (`<wallet>.state.json`) and REFUSES to pay
//!     again while any prior payout is not yet final, so a re-run / crash / cron
//!     overlap cannot double-pay.
//!   * Balance-derived - never pays more than (matured balance - reserve); no
//!     fixed "total" that could overspend.
//!   * Payable-first - the proportional split is computed over payable addresses
//!     only, so unpayable IP-fallback keys never dilute honest miners.
//!   * Chunked - at most 190 recipients per tx, so a large payout is never
//!     rejected by the node's TX_ACTIONS_MAX=200 limit.
//!   * Honest - reports the node's accept/reject for every tx and the real
//!     before/after balances; no "looks funded" guesswork.
//!
//! Usage: hbit-pool-payout <pool_base> <node> <chain> [wallet_file]
//!        [reserve_units] [dust_units] [--commit]
//!   `--help` prints the whole of it: every argument, what it means, and a
//!   working example. See `usage()`, which is the only place that text lives.
//!   `chain` is required - a wrong difficulty rule means rejected blocks.
//!
//! Nothing here ever prompts, so it is safe to run from a script: every input is
//! an argument, and anything missing or wrong is a refusal that says what to do.

use basis::interface::*;
use field::*;
use protocol::action::HacToTrs;
use protocol::transaction::TransactionType2;
use sys::*;

use hbit_pool::difficulty::ChainParams;
use hbit_pool::pool_core::split_payout;
use hbit_pool::{
    Admission, BlockFees, GoneAction, PAYOUT_CHUNK, PAYOUT_DUST_UNITS, PayoutRecord, PayoutTxState,
    SETTLE_RESERVE_UNITS, SubmitVerdict, WALLET_PASSWORD_ENV, acquire_settle_lock, balance,
    block_fees, chunk_tx_fee, classify_payout_tx, confirm_payout, deduct_owed, distributable_units,
    drop_payout, find_u64, get_json, gone_action, http_client, is_payout_address,
    load_immature_blocks, load_or_create_wallet, load_owed, load_paid_ledger, load_payout_records,
    load_pending_payout_txs, load_pplns_credit, merge_payout_rows, mine_and_submit_block, owe_rows,
    payout_amount, pool_state_path, post_hex, save_settlement_ledger, settle_lock_path,
    submit_verdict, take_owed, verify_admitted,
};

/// The default wallet path, used when the operator does not name one. Quoted by
/// `usage()` so the help cannot describe a file the tool does not open.
const DEFAULT_WALLET_FILE: &str = "pool-wallet.key";

/// The leading 16 characters of a tx hash, for readable log lines. Never slices
/// mid-character, so a corrupt ledger entry cannot panic a settlement run.
fn short(hash: &str) -> &str {
    hash.get(..16).unwrap_or(hash)
}

/// Everything needed to run a manual settlement correctly, without reading this
/// source. Printed on `--help` and on any refusal that comes from the command
/// line itself.
fn usage() -> String {
    format!(
        r"hbit-pool-payout v{ver}: pays the pool's miners by hand, out of the pool wallet.
Run it ONLY while hbit-pool-server is stopped, and read the dry run before you commit.

usage:
  hbit-pool-payout <pool_base> <node> <chain> [wallet_file] [reserve_units] [dust_units] [--commit]

  <pool_base>      Base URL of the pool server, e.g. http://127.0.0.1:9777 - the
                   same address you started it on. It is asked for the share
                   window; while the server is stopped, as it must be, that is
                   read from the accounting file next to the wallet instead.

  <node>           Base URL of YOUR OWN Hacash fullnode, already running and
                   synced. Normally http://127.0.0.1:8080 in this package.

  <chain>          Which chain your node is on. REQUIRED and never guessed:
                     mainnet
                     testnet
                     testnet:<difficulty_adjust_blocks>:<each_block_target_time>

  [wallet_file]    The pool's wallet key file. Default {wallet}. This tool never
                   creates one: the pool server does that on its first run.

  [reserve_units]  Units of 0.1 HAC left in the wallet to fund network fees.
                   Default {reserve}, which is what the pool server itself uses
                   and what its /terms endpoint advertises.

  [dust_units]     Units of 0.1 HAC below which a miner is paid nothing this run;
                   the money stays in the wallet for the next one. Default {dust}.

  --commit         Actually pay. WITHOUT IT NOTHING IS PAID: the run prints the
                   split it would make and stops.

the safe order, exactly:
  1. stop hbit-pool-server and wait for the process to really exit
  2. hbit-pool-payout http://127.0.0.1:9777 http://127.0.0.1:8080 mainnet {wallet}
  3. read the planned split; if it is right, run the SAME line again with --commit
  4. start hbit-pool-server again

If the wallet file is encrypted, set {pw} first, in the same window.

The full runbook is POOL-OPERATOR.md.",
        ver = env!("CARGO_PKG_VERSION"),
        wallet = DEFAULT_WALLET_FILE,
        reserve = SETTLE_RESERVE_UNITS,
        dust = PAYOUT_DUST_UNITS,
        pw = WALLET_PASSWORD_ENV,
    )
}

/// Print `text` and stop with the conventional configuration-error status.
fn refuse(text: &str) -> ! {
    eprintln!("{text}");
    std::process::exit(2)
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

/// Remember that the node itself reported holding this payout.
///
/// The one fact that separates "the node lost a transaction it had relayed" from
/// "the node never took it", and therefore the one fact that stops the same
/// window being signed a second time. See [`gone_action`].
fn note_node_holds(records: &mut [PayoutRecord], hash: &str) {
    if let Some(r) = records.iter_mut().find(|r| r.hash == hash) {
        r.node_holds = true;
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.iter()
        .skip(1)
        .any(|x| x == "-h" || x == "--help" || x == "/?")
    {
        println!("{}", usage());
        return;
    }
    let commit = a.iter().any(|x| x == "--commit");
    let pos: Vec<String> = a
        .iter()
        .skip(1)
        .filter(|x| !x.starts_with("--"))
        .cloned()
        .collect();

    // Three arguments are required. Nothing about WHERE the money goes is ever
    // guessed here.
    if pos.len() < 3 {
        eprintln!("{}\n", usage());
        refuse(&format!(
            "REFUSING to run: {} argument(s) given, 3 are required \
             (<pool_base> <node> <chain>).",
            pos.len()
        ));
    }
    let pool_base = pos[0].trim().trim_end_matches('/').to_string();
    let node = pos[1].trim().trim_end_matches('/').to_string();
    let chain = pos[2].trim().to_string();
    // A testnet node reads its difficulty window and block time from its OWN
    // config, so accept them spelled out instead of assuming a pair that would
    // make the confirming block below unmineable.
    let Some(params) = ChainParams::parse(&chain) else {
        eprintln!("{}\n", usage());
        refuse(&format!(
            "REFUSING to run: <chain> must be `mainnet`, `testnet`, or \
             `testnet:<difficulty_adjust_blocks>:<each_block_target_time>` (got `{chain}`).\n\
             What to do: name the chain YOUR node is on, the same one the pool server runs with."
        ));
    };
    let is_testnet = chain != "mainnet";
    let wallet_file = pos
        .get(3)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| DEFAULT_WALLET_FILE.to_string());
    // This tool pays OUT of an existing pool wallet; it must never bring one into
    // being. Creating one here would hand back a fresh empty address, report a
    // zero balance and nothing to pay, and leave a real-money-looking key file in
    // whatever directory the operator happened to be standing in.
    if !std::path::Path::new(&wallet_file).exists() {
        refuse(&format!(
            "REFUSING to run: there is no wallet file at {wallet_file}.\n\
             hbit-pool-server creates the pool wallet on its first run; this tool only ever pays \
             out of one that already exists.\n\
             What to do: run this from the folder that holds the pool's wallet file, or pass the \
             path to it as argument 4."
        ));
    }
    // Default to the pool server's OWN advertised terms, so a manual run pays on
    // the same reserve and the same minimum payout `/terms` states. A value that
    // does not parse is REFUSED rather than quietly replaced by that default:
    // both of these decide how much money stays in the wallet, and an operator
    // who mistyped one would never be told the number they typed was ignored.
    let unit_arg = |i: usize, name: &str, default: u64| -> u64 {
        match pos.get(i) {
            None => default,
            Some(raw) => match raw.trim().parse::<u64>() {
                Ok(v) => v,
                Err(_) => refuse(&format!(
                    "REFUSING to run: [{name}] must be a whole number of 0.1 HAC units \
                     (got `{raw}`).\n\
                     What to do: leave it out to use the pool's own advertised {default}, or pass \
                     a whole number - 5 means 0.5 HAC."
                )),
            },
        }
    };
    let reserve_units = unit_arg(4, "reserve_units", SETTLE_RESERVE_UNITS);
    let dust_units = unit_arg(5, "dust_units", PAYOUT_DUST_UNITS);

    let client = http_client();
    println!(
        "== HBIT pool payout ({}) ==",
        if commit { "COMMIT" } else { "DRY-RUN" }
    );
    // Exclusive claim on this wallet's settlement, held for the whole run. A
    // running hbit-pool-server holds the same lock, so this can never become a second
    // settler paying the same PPLNS window out of the same confirmed balance.
    let _settle_lock = match acquire_settle_lock(&wallet_file) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "REFUSING to run: another hbit-pool-server or hbit-pool-payout already holds \
                 {wallet_file} ({e}).\n\
                 Two settlers would each see the whole wallet balance, each believe it is the only \
                 payer, and pay the same shares twice out of your own funds.\n\
                 What to do: stop hbit-pool-server, wait for it to actually exit, then run this \
                 again.\n\
                 The lock belongs to the running process, not to the file: deleting {} frees \
                 nothing and would only let two of them pay at once.",
                settle_lock_path(&wallet_file)
            );
            std::process::exit(1);
        }
    };
    // The node has to answer before anything is valued. The balance reader now
    // refuses a missing answer on its own, so this check is here for the
    // OPERATOR: it turns "cannot value the wallet" into "your fullnode is not
    // running at this address", which is the sentence somebody can act on.
    if find_u64(
        &get_json(&client, &format!("{node}/query/latest")),
        "height",
    )
    .is_none()
    {
        eprintln!(
            "REFUSING to pay: no Hacash fullnode answered at {node}, so this tool cannot read the \
             wallet's balance or what is already in flight.\n\
             Nothing was paid.\n\
             What to do: start your fullnode, let it finish syncing, and check that {node} is its \
             API address."
        );
        std::process::exit(1);
    }
    let pool_acc = load_or_create_wallet(&wallet_file);
    let pool_addr = pool_acc.readable().to_string();
    let bal = balance(&client, &node, &pool_addr);
    // A balance this tool cannot value is NOT a zero balance: settling on it
    // would sign transactions for a number the node never reported.
    let Some(bal_units) = bal.units() else {
        eprintln!("REFUSING to pay: this tool cannot value the pool wallet ({bal}).");
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
    // The per-worker settlement ledger the pool server keeps. This tool writes to
    // the SAME file, so it has to carry it forward: a payout it makes that is
    // never recorded here is one no miner can ever see it was paid, and a payout
    // that buries while the server is stopped would otherwise vanish out of
    // "in flight" without ever becoming "paid".
    let mut records = load_payout_records(&state_file);
    // What an earlier settlement (by this tool or by the pool server) planned for
    // named miners and failed to deliver. It is paid FIRST below: leaving it to
    // the proportional split would give that money to whoever is in the share
    // window now, including miners whose own chunk went through.
    let mut owed = load_owed(&state_file);
    let mut paid = load_paid_ledger(&state_file);
    if paid.since == 0 {
        paid.since = curtimes();
    }
    let mut prior = load_pending_payout_txs(&state_file);
    // Older builds of this tool kept their OWN ledger, which the server never
    // read. Fold anything left there into the shared one before deciding, so an
    // upgrade cannot lose track of a payout that is still in flight.
    for h in take_legacy_ledger(&wallet_file) {
        if !prior.contains(&h) {
            println!(
                "  adopting payout tx {} from the old private ledger",
                short(&h)
            );
            prior.push(h);
        }
    }
    if !prior.is_empty() {
        let mut still: Vec<String> = Vec::new();
        for h in &prior {
            let j = get_json(&client, &format!("{node}/query/transaction?hash={h}"));
            match classify_payout_tx(&j) {
                // Buried is the ONLY thing that turns money in flight into money
                // paid, and it must be recorded here too: otherwise a payout that
                // confirmed while the pool server was stopped would leave the
                // in-flight list without ever reaching anyone's paid total.
                PayoutTxState::Buried(_) => {
                    if let Some(rec) = confirm_payout(&mut records, &mut paid, h, curtimes()) {
                        println!(
                            "  prior payout tx {} is buried: {} unit(s) to {} miner(s) are now PAID",
                            short(h),
                            rec.units(),
                            rec.rows.len()
                        );
                    }
                }
                // "I do not know that hash" is NOT "nothing was paid". The
                // mempool is memory only, so a node restart empties it and a
                // transaction the node validated, accepted AND RELAYED reads
                // exactly like one it never took.
                PayoutTxState::Gone => match gone_action(records.iter().find(|r| &r.hash == h)) {
                    GoneAction::Forget => {
                        println!(
                            "  prior payout tx {} is unknown to the node, and there are neither \
                             signed bytes for it nor any sighting of the node holding it: nothing \
                             can have been relayed and nobody was paid",
                            short(h)
                        );
                        // Nothing was paid, so nothing is credited. The rows name
                        // the miners it was for, so they become a debt paid
                        // before the fresh split, not money back in the pot.
                        if let Some(rec) = drop_payout(&mut records, h) {
                            owe_rows(&mut owed, &rec.rows);
                        }
                    }
                    GoneAction::Rebroadcast => {
                        // Never re-sign the window. A fresh transaction carries a
                        // fresh timestamp and so a different hash, and replay
                        // protection here is by hash alone: if any peer still
                        // holds the first one, BOTH can be mined and these miners
                        // are paid twice out of the operator's own wallet.
                        let body = records
                            .iter()
                            .find(|r| &r.hash == h)
                            .map(|r| r.body_hex.clone())
                            .unwrap_or_default();
                        let resp = post_hex(
                            &client,
                            &format!("{node}/submit/transaction?hexbody=true"),
                            &body,
                        );
                        println!(
                            "  prior payout tx {} is not in the node's mempool. These bytes were \
                             put on the network, so it can still be mined and this tool will not \
                             re-sign the window. Re-broadcast the identical signed bytes (same \
                             hash) -> {resp}",
                            short(h)
                        );
                        still.push(h.clone());
                    }
                    GoneAction::Stuck => {
                        eprintln!(
                            "  prior payout tx {} left the node's mempool. The node held it once, \
                             so it was relayed and can still be mined, but there are no stored \
                             bytes for it (it predates them) and this tool will NOT re-sign the \
                             window: that pays those miners twice if the first one is ever \
                             included.",
                            short(h)
                        );
                        still.push(h.clone());
                    }
                },
                // The node is telling us it holds this. Record it: `node_holds`
                // is the pool's only memory that a transaction reached the
                // network, and it was written in ONE place - right after a
                // submit whose verification came back `Held`. A payout submitted
                // through a timeout and only seen in the mempool later stayed
                // marked "never held" for ever, so the day the node restarted
                // and answered `Gone` it read as never relayed and its rows were
                // re-issued into a second, differently-hashed transaction.
                PayoutTxState::Confirming(d) => {
                    println!("  prior payout tx {} is only {d} block(s) deep", short(h));
                    note_node_holds(&mut records, h);
                    still.push(h.clone());
                }
                PayoutTxState::Pending => {
                    note_node_holds(&mut records, h);
                    still.push(h.clone());
                }
                PayoutTxState::Unknown => {
                    eprintln!("  cannot determine the state of payout tx {}", short(h));
                    still.push(h.clone());
                }
            }
        }
        if !still.is_empty() {
            if let Err(e) = save_settlement_ledger(&state_file, &still, &records, &owed, &paid) {
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
        if let Err(e) = save_settlement_ledger(&state_file, &[], &records, &owed, &paid) {
            eprintln!("REFUSING to pay: cannot clear the pending ledger {state_file}: {e}");
            std::process::exit(1);
        }
    }

    // 1) PPLNS credit. Try the live pool server first, then fall back to the
    // accounting file it left behind - holding the settlement lock means the
    // server is stopped, so /stats normally cannot answer at all.
    //
    // `credit`, never the `workers` headcount printed beside it: a headcount read
    // at the instant of a payout is a number one miner can own outright by
    // sitting on its shares and dumping a whole window's worth in the second
    // before the split. This tool signs the same transactions the server does, so
    // it has to weigh work the same way or it becomes the way round the fix.
    let stats = get_json(&client, &format!("{pool_base}/stats"));
    let rows = stats
        .get("credit")
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
        counts = load_pplns_credit(&state_file);
        if !counts.is_empty() {
            println!(
                "(pool server not answering; using the share window recorded in {state_file})"
            );
        }
    }
    // An empty window is not enough to stop: a chunk that failed while those
    // miners' shares were in the window is still owed to them long after the
    // window has rolled past, and this file is where that debt lives.
    if counts.is_empty() && owed.is_empty() {
        println!("no shares recorded yet - nothing to pay");
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
    if payable_counts.is_empty() && owed.is_empty() {
        println!("no payable workers (nobody announced a payout address) - nothing to pay");
        return;
    }
    // Apply the SAME maturity gate as the automatic settlement: the pool server
    // records every block it found that is not yet buried, and that income must
    // not be paid out while a reorg could still take it back.
    //
    // Including its TRANSACTION FEES. The chain credits a block's whole fee
    // income to the coinbase address, which is this wallet, so a block the
    // server recorded before it could count them is holding back the subsidy
    // alone and the fees are sitting in the balance below, unaccounted for. This
    // tool runs while the server is stopped, so nothing else is going to count
    // them: it asks the node itself rather than pay them out at zero
    // confirmations.
    let mut immature_units = 0u64;
    for blk in load_immature_blocks(&state_file) {
        immature_units = immature_units.saturating_add(blk.units);
        if blk.fees_counted {
            continue;
        }
        match block_fees(&client, &node, blk.height, &blk.hash) {
            BlockFees::Counted(fee) => immature_units = immature_units.saturating_add(fee),
            // Never landed, or another block took that height: it credited
            // nothing, so there are no fees of its to hold back.
            BlockFees::NotOnChain => {}
            BlockFees::Unknown(why) => {
                eprintln!(
                    "REFUSING to pay: the node could not say what the pool's block at height {} \
                     paid in transaction fees ({why}). That income is already in this wallet, and \
                     splitting a balance that still contains it would pay it out at zero \
                     confirmations - money you would have to fund yourself if the block is \
                     orphaned. Check the node and run this again.",
                    blk.height
                );
                std::process::exit(1);
            }
        }
    }
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
    // Everything a failed chunk owes comes off the top, before a single unit is
    // split. Splitting first would hand that money to the current window, which
    // includes the miners whose own chunks went through: they would be paid twice
    // for the same window, funded by the miners who were paid nothing.
    let (mut split, left) = take_owed(&owed, distributable);
    let owed_now: u64 = split.iter().map(|(_, u)| *u).sum();
    if owed_now > 0 {
        println!(
            "\n{owed_now} unit(s) are OWED to {} miner(s) by an earlier settlement that did not \
             reach the node. They are paid first, before anything is split.",
            split.len()
        );
    }
    split.extend(split_payout(left, 0, dust_units, &payable_counts));
    // One action per miner: a miner that is owed AND has shares in the window is
    // paid once, and every action counts against the node's 200-action limit.
    merge_payout_rows(&mut split);
    if split.is_empty() {
        println!("split produced no payable rows (all below dust {dust_units}) - nothing to pay");
        return;
    }
    let n_tx = split.len().div_ceil(PAYOUT_CHUNK);
    let plan_units: u64 = split.iter().map(|(_, u)| *u).sum();
    println!(
        "\nplanned payment of {plan_units} units over {} miner(s) in {n_tx} tx(s) \
         ({owed_now} of it owed by an earlier settlement, the rest split from this balance):",
        split.len()
    );
    for (w, u) in &split {
        // The chain's own amount, exactly as the transaction will carry it.
        println!("  -> {w} = {}", payout_amount(*u).to_fin_string());
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
        // 0.01 HAC network fee, from the reserve, built by the same helper the
        // pool server and `/terms` use.
        let mut tx = TransactionType2::new_by(main.clone(), chunk_tx_fee(), curtimes());
        // Exactly what this transaction pays, so a miner can later be told what
        // it was paid and by which transaction. Only rows that really made it
        // into the transaction are recorded.
        let mut rows: Vec<(String, u64)> = Vec::with_capacity(chunk.len());
        for (worker, units) in chunk {
            let Ok(to) = Address::from_readable(worker) else {
                continue;
            };
            let mut act = HacToTrs::new();
            act.to = AddrOrPtr::from_addr(to);
            act.hacash = payout_amount(*units);
            if tx.push_action(Box::new(act)).is_err() {
                break;
            }
            rows.push((worker.clone(), *units));
        }
        let pushed = rows.len();
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
        // pool server) could pay all over again. The per-recipient rows go in the
        // same write: a tracked hash with no rows behind it is a payout no miner
        // could ever be shown.
        let txhash = hex::encode(tx.hash().serialize());
        // Serialized before the record is written, because the record carries it:
        // these bytes are the only way to put this exact transaction back on the
        // network if the node later loses it, and the only alternative is signing
        // a second transaction for the same window that can also be mined.
        let body_hex = hex::encode(tx.serialize());
        submitted.push(txhash.clone());
        // What this chunk pays comes off the owed ledger in the same write. If
        // the chunk then fails, the rows go back on it below.
        deduct_owed(&mut owed, &rows);
        records.push(PayoutRecord {
            hash: txhash.clone(),
            at: curtimes(),
            node_holds: false,
            body_hex: body_hex.clone(),
            rows,
        });
        if let Err(e) = save_settlement_ledger(&state_file, &submitted, &records, &owed, &paid) {
            eprintln!(
                "  cannot record the payout tx in {state_file} ({e}); ABORTING before submit so \
                 nothing is paid untracked."
            );
            std::process::exit(1);
        }

        let resp = post_hex(
            &client,
            &format!("{node}/submit/transaction?hexbody=true"),
            &body_hex,
        );
        match submit_verdict(&resp) {
            SubmitVerdict::Accepted => {}
            SubmitVerdict::Rejected => {
                all_ok = false;
                println!(
                    "  tx {} paying {pushed} miner(s): REJECTED -> {resp}",
                    short(&txhash)
                );
                // The node's own validator refused it, so it never inserted it
                // and never relayed it. Nothing was paid, so the rows come out
                // with it - onto the owed ledger, which the next settlement pays
                // before it splits anything.
                submitted.retain(|h| h != &txhash);
                if let Some(rec) = drop_payout(&mut records, &txhash) {
                    owe_rows(&mut owed, &rec.rows);
                }
                let _ = save_settlement_ledger(&state_file, &submitted, &records, &owed, &paid);
                continue;
            }
            SubmitVerdict::Unresolved => {
                // A timeout or a dropped connection is NOT a refusal: the node
                // may have taken these bytes, inserted them and relayed them
                // before the answer was lost. Forgetting the hash here is how the
                // same window gets signed a second time and paid twice.
                all_ok = false;
                println!(
                    "  tx {} paying {pushed} miner(s): no usable answer -> {resp}\n\
                     the node may hold it, so it stays in the pending ledger and is NOT re-issued",
                    short(&txhash)
                );
                continue;
            }
        }
        // ret=0 only means the API took the bytes. The node validates the
        // transaction synchronously and then inserts it into the mempool on a
        // background task whose result it DISCARDS, so an accepted response is
        // no evidence at all. Ask the node what it actually holds.
        let held = match verify_admitted(&client, &node, &txhash) {
            Admission::Held => {
                println!(
                    "  tx {} paying {pushed} miner(s): the node holds it",
                    short(&txhash)
                );
                if let Some(r) = records.iter_mut().find(|r| r.hash == txhash) {
                    r.node_holds = true;
                }
                let _ = save_settlement_ledger(&state_file, &submitted, &records, &owed, &paid);
                true
            }
            Admission::Missing => {
                all_ok = false;
                println!(
                    "  tx {} paying {pushed} miner(s): the API accepted it but the node does NOT \
                     hold it - nothing was paid and nothing was relayed. These rows are now OWED \
                     and the next settlement pays them first.",
                    short(&txhash)
                );
                submitted.retain(|h| h != &txhash);
                if let Some(rec) = drop_payout(&mut records, &txhash) {
                    owe_rows(&mut owed, &rec.rows);
                }
                let _ = save_settlement_ledger(&state_file, &submitted, &records, &owed, &paid);
                false
            }
            Admission::Unresolved => {
                all_ok = false;
                println!(
                    "  tx {} paying {pushed} miner(s): submitted, but the node's verdict could \
                     not be read - keeping it in the pending ledger",
                    short(&txhash)
                );
                false
            }
        };

        // On testnet the pool has no other miners, so self-mine a confirming
        // block that includes this tx. On mainnet the tx waits in the mempool for
        // the network to include it (the pool mines coinbase-only blocks).
        if is_testnet && held {
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
        println!("\nEvery payout tx is in the node. NOTHING IS PAID until a block includes them:");
        println!("they stay in the shared pending ledger until they are buried deep enough that a");
        println!("reorg cannot undo them; re-running before then is safe: this tool and the pool");
        println!("server both refuse to double-pay.");
    } else {
        eprintln!("\nSome payout tx(s) never reached the node, failed to sign, or could not be");
        eprintln!("verified - see above. Those miners are still owed, and the exact rows are");
        eprintln!("recorded in the owed ledger in the pool state file: the next settlement, by");
        eprintln!("this tool or by the pool server, pays THEM first, before it splits anything");
        eprintln!("fresh. The pending ledger keeps every hash the node might hold, so a retry");
        eprintln!("cannot double-pay them either.");
    }
    println!(
        "pool wallet after = {}",
        balance(&client, &node, &pool_addr)
    );
}
