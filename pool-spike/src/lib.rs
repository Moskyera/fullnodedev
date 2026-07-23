//! Shared helpers for the pool spikes: HTTP glue + off-node block assembly that
//! mirrors the node's `impl_packing_next_block` for a block containing a
//! coinbase plus optional extra transactions. Targets a fresh local testnet
//! (bootstrap LOWEST_DIFFICULTY); does not reproduce mainnet ASERT difficulty.

pub mod difficulty;
pub mod pool_core;

use difficulty::ChainParams;

use basis::difficulty::*;
use basis::interface::*;
use field::*;
use protocol::block::*;
use protocol::transaction::*;
use sys::*;

use serde_json::Value;

pub fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .expect("http client")
}

pub fn get_json(client: &reqwest::blocking::Client, url: &str) -> Value {
    let text = client
        .get(url)
        .send()
        .and_then(|r| r.text())
        .unwrap_or_else(|e| format!("{{\"http_error\":\"{e}\"}}"));
    serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text))
}

pub fn post_hex(client: &reqwest::blocking::Client, url: &str, body: &str) -> String {
    client
        .post(url)
        .header("content-type", "text/plain")
        .body(body.to_string())
        .send()
        .and_then(|r| r.text())
        .unwrap_or_else(|e| format!("http_error: {e}"))
}

pub fn find_u64(v: &Value, key: &str) -> Option<u64> {
    find_value(v, key).and_then(|x| {
        x.as_u64()
            .or_else(|| x.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

pub fn find_str(v: &Value, key: &str) -> Option<String> {
    find_value(v, key).and_then(|x| x.as_str().map(|s| s.to_string()))
}

pub fn find_value<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|child| find_value(child, key))),
        Value::Array(arr) => arr.iter().find_map(|child| find_value(child, key)),
        _ => None,
    }
}

/// The recipient's "hacash" balance string (e.g. "1:248"), or "" if none.
pub fn balance(client: &reqwest::blocking::Client, base: &str, addr: &str) -> String {
    let j = get_json(client, &format!("{base}/query/balance?address={addr}"));
    find_str(&j, "hacash").unwrap_or_default()
}

/// Is this string a payable Hacash address (normal single-key PRIVAKEY)?
/// Workers announce one as `&worker=<address>`; the pool then uses the address
/// itself as the share-accounting key, so payouts need no name->address map.
pub fn is_payout_address(s: &str) -> bool {
    Address::from_readable(s)
        .map(|a| a.is_privakey())
        .unwrap_or(false)
}

/// Load the pool wallet from `path` (a file holding a 64-hex secp256k1 private
/// key), creating a fresh random one if the file does not exist. The private key
/// only ever lives in that file — it is never printed or logged; only the
/// address is shown.
pub fn load_or_create_wallet(path: &str) -> Account {
    if let Ok(txt) = std::fs::read_to_string(path) {
        let key_hex = txt.trim().to_string();
        assert_eq!(
            key_hex.len(),
            64,
            "wallet file {path} must hold a 64-hex private key"
        );
        let acc = Account::create_by(&key_hex).expect("invalid key in wallet file");
        println!("pool wallet {} (from {path})", acc.readable());
        return acc;
    }
    // No wallet yet: generate one and persist it.
    let acc = loop {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("system RNG");
        if let Ok(a) = Account::create_by_secret_key_value(key) {
            break a;
        }
    };
    std::fs::write(path, format!("{}\n", hex::encode(acc.secret_key().serialize())))
        .expect("write wallet file");
    println!("CREATED A NEW POOL WALLET -> {path}");
    println!("  address: {}", acc.readable());
    println!("  BACK UP THAT FILE. Whoever holds it controls the pool's funds.");
    acc
}

/// Everything the pool needs to build and verify blocks for the current tip.
/// The pool serves one template to all workers; each worker gets its own
/// extranonce (the coinbase `miner_nonce`), which changes the merkle root and
/// therefore gives every worker a private search space.
#[derive(Clone)]
pub struct Template {
    pub height: u64,
    pub prevhash: Hash,
    pub timestamp: u64,
    /// Header `difficulty` field (u32) — must equal what the node recomputes.
    pub difficulty: u32,
    /// The exact PoW target for this block. NOT interchangeable with
    /// u32_to_hash(difficulty): on the from_big path it is more precise.
    pub target: [u8; 32],
    pub coinbase_addr: Address,
}

/// Read the chain tip and build a template for the next block, computing the
/// next difficulty off-node with the same rule the node will validate against.
pub fn fetch_template(
    client: &reqwest::blocking::Client,
    base: &str,
    coinbase_addr: &str,
    params: &ChainParams,
) -> Template {
    let latest = get_json(client, &format!("{base}/query/latest"));
    let prev_hei = find_u64(&latest, "height").expect("no 'height' in /query/latest");
    let height = prev_hei + 1;
    let (prevhash, prev_ts, prev_diff) = if prev_hei == 0 {
        (mint::genesis::genesis_block_hash(), 1549250700u64, 0u32)
    } else {
        let ij = get_json(client, &format!("{base}/query/block/intro?height={prev_hei}"));
        let ph = find_str(&ij, "hash").expect("no 'hash' in block intro");
        (
            Hash::from_hex(ph.as_bytes()).expect("bad prevhash hex"),
            find_u64(&ij, "timestamp").unwrap_or(0),
            find_u64(&ij, "difficulty").unwrap_or(0) as u32,
        )
    };
    let timestamp = std::cmp::max(curtimes(), prev_ts.saturating_add(1));
    // ASERT anchors on the activation block's timestamp; only needed above it.
    let anchor_time = if params.needs_anchor(height) {
        let aj = get_json(
            client,
            &format!("{base}/query/block/intro?height={}", params.asert_height),
        );
        find_u64(&aj, "timestamp").expect("anchor block timestamp")
    } else {
        0
    };
    let (diff_num, target) =
        difficulty::next_difficulty(params, height, timestamp, prev_diff, anchor_time);
    Template {
        height,
        prevhash,
        timestamp,
        difficulty: diff_num,
        target,
        coinbase_addr: Address::from_readable(coinbase_addr).expect("bad coinbase address"),
    }
}

/// The template's coinbase carrying `extranonce` in its miner_nonce field.
pub fn coinbase_with_extranonce(tpl: &Template, extranonce: &[u8; 32]) -> mint::TransactionCoinbase {
    let mut cb = mint::create_coinbase_tx(tpl.height, Fixed16::default(), tpl.coinbase_addr.clone());
    let en = Hash::from_hex(hex::encode(extranonce).as_bytes()).expect("extranonce");
    cb.extend = mint::CoinbaseExtend::must(mint::CoinbaseExtendDataV1 {
        miner_nonce: en,
        witness_count: Uint1::from(0),
    });
    cb
}

fn build_intro(tpl: &Template, cb: &mint::TransactionCoinbase, nonce: u32) -> BlockIntro {
    BlockIntro {
        head: BlockHead {
            version: Uint1::from(1),
            height: BlockHeight::from(tpl.height),
            timestamp: Timestamp::from(tpl.timestamp),
            prevhash: tpl.prevhash.clone(),
            mrklroot: calculate_mrklroot(&vec![cb.hash_with_fee()]),
            transaction_count: Uint4::from(1u32),
        },
        meta: BlockMeta {
            nonce: Uint4::from(nonce),
            difficulty: Uint4::from(tpl.difficulty),
            witness_stage: Fixed2::default(),
        },
    }
}

/// The 89-byte block header a worker hashes (nonce lives at bytes 79..83).
pub fn intro_bytes(tpl: &Template, cb: &mint::TransactionCoinbase, nonce: u32) -> Vec<u8> {
    build_intro(tpl, cb, nonce).serialize()
}

/// Hex of the serialized coinbase tx — the `coinbase_body` a worker receives.
/// Its optional `extend` block must be present or the worker's own
/// `set_mining_nonce` becomes a silent no-op (all threads would then share one
/// coinbase hash); `create_coinbase_tx` always emits it.
pub fn coinbase_body_hex(cb: &mint::TransactionCoinbase) -> String {
    hex::encode(cb.serialize())
}

/// Serialized full block for a winning (extranonce, nonce).
pub fn assemble_block(tpl: &Template, cb: &mint::TransactionCoinbase, nonce: u32) -> Vec<u8> {
    let mut txs = DynVecTransaction::default();
    txs.push(Box::new(cb.clone())).expect("push coinbase");
    BlockV1 {
        intro: build_intro(tpl, cb, nonce),
        transactions: txs,
    }
    .serialize()
}

/// Submit already-serialized block bytes.
pub fn submit_block_bytes(
    client: &reqwest::blocking::Client,
    base: &str,
    bytes: &[u8],
) -> String {
    post_hex(
        client,
        &format!("{base}/submit/block?hexbody=true"),
        &hex::encode(bytes),
    )
}

/// Assemble a block whose coinbase pays `coinbase_addr`, plus `extra_txs`,
/// CPU-mine it at bootstrap difficulty, and submit via /submit/block.
/// Returns (next_height, submit_response).
pub fn mine_and_submit_block(
    client: &reqwest::blocking::Client,
    base: &str,
    coinbase_addr: &str,
    extra_txs: Vec<Box<dyn Transaction>>,
    params: &ChainParams,
) -> (u64, String) {
    let tpl = fetch_template(client, base, coinbase_addr, params);
    let cbtx = mint::create_coinbase_tx(tpl.height, Fixed16::default(), tpl.coinbase_addr.clone());

    let mut trshxs: Vec<Hash> = vec![cbtx.hash_with_fee()];
    let mut transactions = DynVecTransaction::default();
    transactions.push(Box::new(cbtx.clone())).expect("push coinbase");
    for tx in extra_txs {
        trshxs.push(tx.hash_with_fee());
        transactions.push(tx).expect("push extra tx");
    }
    let count = trshxs.len() as u32;

    let mut intro = BlockIntro {
        head: BlockHead {
            version: Uint1::from(1),
            height: BlockHeight::from(tpl.height),
            timestamp: Timestamp::from(tpl.timestamp),
            prevhash: tpl.prevhash.clone(),
            mrklroot: calculate_mrklroot(&trshxs),
            transaction_count: Uint4::from(count),
        },
        meta: BlockMeta {
            nonce: Uint4::default(),
            difficulty: Uint4::from(tpl.difficulty),
            witness_stage: Fixed2::default(),
        },
    };

    let mut nonce: u32 = 0;
    loop {
        intro.meta.nonce = Uint4::from(nonce);
        let ph = x16rs::block_hash(tpl.height, &intro.serialize());
        if !hash_bigger_than(&ph, &tpl.target) {
            break;
        }
        nonce = nonce.wrapping_add(1);
        if nonce == 0 {
            // Never roll the timestamp here: under ASERT the difficulty is a
            // function of this block's own timestamp, so changing it would make
            // the header's difficulty field wrong. Ask for a fresh template.
            return (
                tpl.height,
                "{\"ok\":false,\"err\":\"nonce space exhausted; re-fetch template\"}".to_string(),
            );
        }
    }

    let block = BlockV1 { intro, transactions };
    let resp = post_hex(
        client,
        &format!("{base}/submit/block?hexbody=true"),
        &hex::encode(block.serialize()),
    );
    (tpl.height, resp)
}
