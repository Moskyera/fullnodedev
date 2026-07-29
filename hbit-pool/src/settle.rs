//! P1 settlement proof: build + SIGN + submit ONE transaction that pays MANY
//! recipients FRACTIONAL amounts (the pool's "batched settlement", up to 200
//! outputs), then mine a block that includes it so the payouts confirm. Proves
//! the on-chain "payout" half of the pool. Sender + recipients are deterministic
//! accounts we control, so balances are verifiable.
//!
//! Usage:  hbit-settle-spike [node_base_url]

use std::env;

use basis::interface::*;
use field::*;
use protocol::action::HacToTrs;
use protocol::transaction::TransactionType2;
use sys::*;

use hbit_pool::{BalanceAnswer, balance, http_client, mine_and_submit_block, post_hex};

/// Does this address hold anything at all? The spike pays FRACTIONAL amounts,
/// most of them under 0.1 HAC, so it reads the node's own string rather than the
/// 0.1-HAC valuation the payout path uses - that one floors 0.3 HAC to nothing.
///
/// A node that did not answer is NOT funded here. Reading silence as "paid"
/// would let this spike report SUCCESS without a single balance having moved.
fn is_funded(b: &BalanceAnswer) -> bool {
    matches!(b, BalanceAnswer::Reported(s) if !s.starts_with("0:"))
}

fn main() {
    let base = env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:8088".to_string());
    let base = base.trim_end_matches('/').to_string();

    // This is a LOCAL TESTNET demo: it spends from and pays to well-known
    // deterministic accounts ([1..4;32]) whose private keys are public. It must
    // NEVER be pointed at mainnet or any chain holding real value. Require an
    // explicit `testnet` confirmation so it cannot be run against real money by
    // accident.
    if env::args().nth(2).as_deref() != Some("testnet") {
        eprintln!(
            "hbit-settle-spike is a TESTNET-ONLY demo that uses well-known public keys ([1..4;32]).\n\
             It must never touch mainnet. Re-run as:  hbit-settle-spike <node_base_url> testnet"
        );
        std::process::exit(2);
    }

    let client = http_client();

    // Deterministic accounts we control (public keys — testnet demo only).
    let sender = Account::create_by_secret_key_value([1u8; 32]).expect("sender account");
    let recipients: Vec<(Account, &str)> = vec![
        (
            Account::create_by_secret_key_value([2u8; 32]).unwrap(),
            "2:247",
        ), // 0.2 HAC
        (
            Account::create_by_secret_key_value([3u8; 32]).unwrap(),
            "3:247",
        ), // 0.3 HAC
        (
            Account::create_by_secret_key_value([4u8; 32]).unwrap(),
            "1:247",
        ), // 0.1 HAC
    ];

    println!("== HBIT settlement spike ==");
    println!("node   = {base}");
    println!("sender = {}", sender.readable());
    let sender_bal = balance(&client, &base, sender.readable());
    println!("sender balance = {sender_bal}");

    if !is_funded(&sender_bal) {
        println!(
            "\nSender is unfunded. Fund it by mining one block to it, then re-run:\n  \
             hbit-pool-spike {base} {}\n",
            sender.readable()
        );
        return;
    }

    // Build ONE Type2 transaction paying all recipients (implicit FROM = main).
    let main = Address::from(*sender.address());
    let fee = Amount::from("1:246").expect("fee"); // 0.01 HAC
    let ts = curtimes();
    let mut tx = TransactionType2::new_by(main, fee, ts);

    println!("\nbuilding transfer with {} recipients:", recipients.len());
    for (rec, amt_str) in &recipients {
        let to = Address::from_readable(rec.readable()).expect("recipient address");
        let amt = Amount::from(amt_str).expect("amount");
        let mut act = HacToTrs::new();
        act.to = AddrOrPtr::from_addr(to);
        act.hacash = amt;
        tx.push_action(Box::new(act)).expect("push action");
        println!("  -> {} {amt_str}", rec.readable());
    }

    // Sign once (sender == main => signs hash_with_fee).
    tx.fill_sign(&sender).expect("fill_sign");
    let body_hex = hex::encode(tx.serialize());
    println!("signed tx bytes = {}", body_hex.len() / 2);

    println!("\nbefore:");
    for (rec, _) in &recipients {
        println!(
            "  {} = {}",
            rec.readable(),
            balance(&client, &base, rec.readable())
        );
    }

    // (a) submit to the mempool — the pool's normal action.
    let resp = post_hex(
        &client,
        &format!("{base}/submit/transaction?hexbody=true"),
        &body_hex,
    );
    println!("\n/submit/transaction -> {resp}");

    // (b) confirm it by mining a block that INCLUDES the transfer (this testnet
    //     has no miner of its own), so the payouts actually land.
    let (h, blkresp) = mine_and_submit_block(
        &client,
        &base,
        sender.readable(),
        vec![Box::new(tx) as Box<dyn Transaction>],
        &hbit_pool::difficulty::ChainParams::from_name("testnet"),
    );
    println!("mined confirming block {h} (coinbase+transfer) -> {blkresp}");

    // Verify recipient balances after.
    let mut after: Vec<BalanceAnswer> = Vec::new();
    for _ in 0..12 {
        after = recipients
            .iter()
            .map(|(rec, _)| balance(&client, &base, rec.readable()))
            .collect();
        if after.iter().all(is_funded) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(800));
    }

    println!("\nafter:");
    let mut all_paid = true;
    for ((rec, want), got) in recipients.iter().zip(after.iter()) {
        let ok = is_funded(got);
        all_paid &= ok;
        println!(
            "  {} = {got}  (wanted {want}) {}",
            rec.readable(),
            if ok { "OK" } else { "--" }
        );
    }
    println!("  sender = {}", balance(&client, &base, sender.readable()));
    if all_paid {
        println!(
            "\nSUCCESS: one signed tx paid all {} recipients fractional amounts, confirmed on-chain.",
            recipients.len()
        );
    } else {
        println!("\nNot all recipients funded yet — check the responses above.");
    }
}
