//! Rig helper: derive an address from a throwaway secret, and push real
//! transfers into a node's mempool so a mined block CONTAINS TRANSACTIONS.
//!
//! This exists because the pool's transaction-fee hold-back can only be
//! exercised by a block that actually carries fee-paying transactions, and a
//! private rig chain has no other traffic on it.
//!
//! TESTNET ONLY. It takes a raw private key on the command line.
//!
//! usage:
//!   rig_tx addr <secret_hex_32b>
//!   rig_tx send <node> <secret_hex_32b> <to_address> <hac_amount> <fee_amount> [count]
//!
//! `addr` prints the readable address for a secret so it can be funded.
//! `send`  builds, signs and submits `count` transfers (each with a distinct
//!         timestamp so the hashes differ), printing every node answer.

use basis::interface::*;
use field::*;
use protocol::action::HacToTrs;
use protocol::transaction::TransactionType2;
use sys::*;

use hbit_pool::{get_json, http_client, post_hex};

fn secret_from_hex(s: &str) -> [u8; 32] {
    let b = hex::decode(s).expect("secret must be 64 hex chars");
    assert_eq!(b.len(), 32, "secret must be 32 bytes");
    let mut k = [0u8; 32];
    k.copy_from_slice(&b);
    k
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(|s| s.as_str()) {
        Some("addr") => {
            let acc = Account::create_by_secret_key_value(secret_from_hex(&a[2])).expect("account");
            println!("{}", acc.readable());
        }
        Some("send") => {
            let base = a[2].trim_end_matches('/').to_string();
            let acc = Account::create_by_secret_key_value(secret_from_hex(&a[3])).expect("account");
            let to = Address::from_readable(&a[4]).expect("to_address");
            let amt = Amount::from(&a[5]).expect("hac amount");
            let fee = Amount::from(&a[6]).expect("fee amount");
            let count: u64 = a.get(7).and_then(|s| s.parse().ok()).unwrap_or(1);

            let client = http_client();
            let main = Address::from(*acc.address());
            println!("from   = {}", acc.readable());
            println!("to     = {}", a[4]);
            println!(
                "amount = {} each, fee = {} each, count = {count}",
                a[5], a[6]
            );

            let base_ts = curtimes();
            for i in 0..count {
                // Distinct timestamps make the hashes differ. They go BACKWARDS:
                // the node refuses a transaction stamped later than its own
                // clock, so counting up rejects everything after the first.
                let mut tx =
                    TransactionType2::new_by(main.clone(), fee.clone(), base_ts.saturating_sub(i));
                let mut act = HacToTrs::new();
                act.to = AddrOrPtr::from_addr(to.clone());
                act.hacash = amt.clone();
                tx.push_action(Box::new(act)).expect("push action");
                tx.fill_sign(&acc).expect("fill_sign");
                let hash = hex::encode(tx.hash().serialize());
                let body = hex::encode(tx.serialize());
                let resp = post_hex(
                    &client,
                    &format!("{base}/submit/transaction?hexbody=true"),
                    &body,
                );
                println!("submit {hash} -> {resp}");
            }
            // Report what the node thinks it now holds, so "submitted" is never
            // confused with "in the mempool".
            std::thread::sleep(std::time::Duration::from_millis(800));
            let pend = get_json(
                &client,
                &format!("{base}/query/miner/pending?detail=true&transaction=true&stuff=true"),
            );
            let n = pend
                .get("data")
                .and_then(|d| d.get("transactions"))
                .and_then(|v| v.as_array())
                .map(|a| a.len());
            println!("node template now carries transactions: {n:?}");
        }
        _ => {
            eprintln!(
                "usage:\n  rig_tx addr <secret_hex_32b>\n  rig_tx send <node> <secret_hex_32b> <to_address> <hac> <fee> [count]"
            );
            std::process::exit(2);
        }
    }
}
