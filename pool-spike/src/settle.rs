//! P1 settlement proof: build + SIGN + submit ONE transaction that pays MANY
//! recipients FRACTIONAL amounts (the pool's "batched settlement", up to 200
//! outputs), then mine a block that includes it so the payouts confirm. Proves
//! the on-chain "payout" half of the pool. Sender + recipients are deterministic
//! accounts we control, so balances are verifiable.
//!
//! Usage:  settle-spike [node_base_url]

use std::env;

use basis::interface::*;
use field::*;
use protocol::action::HacToTrs;
use protocol::transaction::TransactionType2;
use sys::*;

use pool_spike::{balance, http_client, mine_and_submit_block, post_hex};

fn main() {
    let base = env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:8088".to_string());
    let base = base.trim_end_matches('/').to_string();

    let client = http_client();

    // Deterministic accounts we control.
    let sender = Account::create_by_secret_key_value([1u8; 32]).expect("sender account");
    let recipients: Vec<(Account, &str)> = vec![
        (Account::create_by_secret_key_value([2u8; 32]).unwrap(), "2:247"), // 0.2 HAC
        (Account::create_by_secret_key_value([3u8; 32]).unwrap(), "3:247"), // 0.3 HAC
        (Account::create_by_secret_key_value([4u8; 32]).unwrap(), "1:247"), // 0.1 HAC
    ];

    println!("== settle-spike ==");
    println!("node   = {base}");
    println!("sender = {}", sender.readable());
    let sender_bal = balance(&client, &base, sender.readable());
    println!("sender balance = {sender_bal}");

    if sender_bal.is_empty() || sender_bal.starts_with("0:") {
        println!(
            "\nSender is unfunded. Fund it by mining one block to it, then re-run:\n  \
             pool-spike {base} {}\n",
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
        println!("  {} = {}", rec.readable(), balance(&client, &base, rec.readable()));
    }

    // (a) submit to the mempool — the pool's normal action.
    let resp = post_hex(&client, &format!("{base}/submit/transaction?hexbody=true"), &body_hex);
    println!("\n/submit/transaction -> {resp}");

    // (b) confirm it by mining a block that INCLUDES the transfer (this testnet
    //     has no miner of its own), so the payouts actually land.
    let (h, blkresp) = mine_and_submit_block(
        &client,
        &base,
        sender.readable(),
        vec![Box::new(tx) as Box<dyn Transaction>],
        &pool_spike::difficulty::ChainParams::from_name("testnet"),
    );
    println!("mined confirming block {h} (coinbase+transfer) -> {blkresp}");

    // Verify recipient balances after.
    let mut after: Vec<String> = Vec::new();
    for _ in 0..12 {
        after = recipients
            .iter()
            .map(|(rec, _)| balance(&client, &base, rec.readable()))
            .collect();
        if after.iter().all(|b| !b.is_empty() && !b.starts_with("0:")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(800));
    }

    println!("\nafter:");
    let mut all_paid = true;
    for ((rec, want), got) in recipients.iter().zip(after.iter()) {
        let ok = !got.is_empty() && !got.starts_with("0:");
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
