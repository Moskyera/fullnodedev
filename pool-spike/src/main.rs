//! P1 feasibility spike for the community pool.
//!
//! Proves the make-or-break claim: a process OUTSIDE the node can assemble a
//! valid block whose coinbase pays a CHOSEN address, CPU-mine it, and have the
//! node accept it via POST /submit/block — with no node change.
//!
//! It mirrors the node's own `impl_packing_next_block`
//! (mint/src/check/block_build.rs) for a coinbase-only block, then mines and
//! submits. Run it against a FRESH LOCAL TESTNET (chain_id != 0), where the
//! first ~289 blocks use LOWEST_DIFFICULTY and a single CPU core finds a block
//! near-instantly. It does NOT reproduce mainnet ASERT difficulty (out of scope
//! for the spike).
//!
//! Usage:  pool-spike [node_base_url] [payout_privakey_address]
//! e.g.    pool-spike http://127.0.0.1:8088 1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9

use std::env;
use std::time::Duration;

use basis::difficulty::*;
use basis::interface::*;
use field::*;
use protocol::block::*;
use protocol::transaction::*;
use sys::*;

use mint::create_coinbase_tx;

use serde_json::Value;

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

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("http client");

    // 1) chain tip height
    let latest = get_json(&client, &format!("{base}/query/latest"));
    println!("/query/latest -> {latest}");
    let prev_hei = find_u64(&latest, "height").expect("no 'height' in /query/latest");
    println!("prev height = {prev_hei}");

    // 2) previous block: its hash is the next block's prevhash. Genesis (height 0)
    //    is not served by /query/block/intro, so take it from the linked genesis
    //    constant (genesis_block_hash() just parses the constant — no block hasher).
    let next_hei = prev_hei + 1;
    let (prevhash, prev_ts) = if prev_hei == 0 {
        println!("prev = linked genesis (height 0)");
        (mint::genesis::genesis_block_hash(), 1549250700u64)
    } else {
        let intro_j = get_json(&client, &format!("{base}/query/block/intro?height={prev_hei}"));
        println!("/query/block/intro?height={prev_hei} -> {intro_j}");
        let ph = find_str(&intro_j, "hash").expect("no 'hash' in block intro");
        let ts = find_u64(&intro_j, "timestamp").unwrap_or(0);
        (Hash::from_hex(ph.as_bytes()).expect("bad prevhash hex"), ts)
    };

    // Fresh-testnet bootstrap difficulty (heights <= window+1 use LOWEST_DIFFICULTY).
    let diff: u32 = LOWEST_DIFFICULTY;
    let next_ts = std::cmp::max(curtimes(), prev_ts.saturating_add(1));
    println!("assembling block height={next_hei} difficulty={diff} timestamp={next_ts}");

    // 3) coinbase paying OUR chosen address
    let adr = Address::from_readable(&payout).expect("bad payout address");
    assert!(
        adr.is_privakey(),
        "payout address must be a PRIVAKEY (version-0) address"
    );
    let cbtx = create_coinbase_tx(next_hei, Fixed16::default(), adr);
    println!("coinbase built for height {next_hei}");

    // 4) assemble BlockV1 (coinbase-only) — mirror of impl_packing_next_block
    let trshxs: Vec<Hash> = vec![cbtx.hash_with_fee()];
    let mut transactions = DynVecTransaction::default();
    transactions
        .push(Box::new(cbtx.clone()))
        .expect("push coinbase");
    let mut intro = BlockIntro {
        head: BlockHead {
            version: Uint1::from(1),
            height: BlockHeight::from(next_hei),
            timestamp: Timestamp::from(next_ts),
            prevhash,
            mrklroot: calculate_mrklroot(&trshxs),
            transaction_count: Uint4::from(1u32),
        },
        meta: BlockMeta {
            nonce: Uint4::default(),
            difficulty: Uint4::from(diff),
            witness_stage: Fixed2::default(),
        },
    };

    // 5) CPU-mine: vary the header nonce until the x16rs block hash beats target.
    let target = DifficultyTarget::from_num(diff).hash;
    let mut nonce: u32 = 0;
    let found: [u8; 32];
    loop {
        intro.meta.nonce = Uint4::from(nonce);
        let ph = x16rs::block_hash(next_hei, &intro.serialize());
        if !hash_bigger_than(&ph, &target) {
            found = ph;
            break;
        }
        nonce = nonce.wrapping_add(1);
        if nonce == 0 {
            // Exhausted 2^32 nonces; refresh the timestamp for a new search space.
            intro.head.timestamp = Timestamp::from(curtimes());
        }
        if nonce % 1_000_000 == 0 {
            println!("mining... nonce={nonce}");
        }
    }
    println!("MINED: nonce={nonce} hash={}", hex::encode(found));

    // 6) serialize + submit
    let block = BlockV1 { intro, transactions };
    let bytes = block.serialize();
    println!("block bytes = {} (submitting hex)", bytes.len());
    let resp = post_hex(
        &client,
        &format!("{base}/submit/block?hexbody=true"),
        &hex::encode(&bytes),
    );
    println!("/submit/block -> {resp}");

    // 7) verify the new tip
    let check = get_json(&client, &format!("{base}/query/block/intro?height={next_hei}"));
    println!("/query/block/intro?height={next_hei} -> {check}");
    match find_str(&check, "miner") {
        Some(m) if m == payout => println!("\nSUCCESS: block {next_hei} accepted, coinbase pays {m}"),
        Some(m) => println!("\nblock {next_hei} present but coinbase pays {m} (expected {payout})"),
        None => println!("\nblock {next_hei} not found yet — check the submit response above"),
    }
}

fn get_json(client: &reqwest::blocking::Client, url: &str) -> Value {
    let text = client
        .get(url)
        .send()
        .and_then(|r| r.text())
        .unwrap_or_else(|e| format!("{{\"http_error\":\"{e}\"}}"));
    serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text))
}

fn post_hex(client: &reqwest::blocking::Client, url: &str, body: &str) -> String {
    client
        .post(url)
        .header("content-type", "text/plain")
        .body(body.to_string())
        .send()
        .and_then(|r| r.text())
        .unwrap_or_else(|e| format!("http_error: {e}"))
}

/// Recursively find the first value for `key` anywhere in the JSON, as u64
/// (accepts a JSON number or a numeric string).
fn find_u64(v: &Value, key: &str) -> Option<u64> {
    find_value(v, key).and_then(|x| {
        x.as_u64()
            .or_else(|| x.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

fn find_str(v: &Value, key: &str) -> Option<String> {
    find_value(v, key).and_then(|x| x.as_str().map(|s| s.to_string()))
}

fn find_value<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(map) => {
            if let Some(found) = map.get(key) {
                return Some(found);
            }
            for (_, child) in map {
                if let Some(found) = find_value(child, key) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => arr.iter().find_map(|child| find_value(child, key)),
        _ => None,
    }
}
