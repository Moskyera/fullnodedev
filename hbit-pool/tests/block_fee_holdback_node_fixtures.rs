//! The maturity hold-back must cover a found block's TRANSACTION FEES, not just
//! its coinbase subsidy, and it must never read an answer it cannot understand
//! as "no fees".
//!
//! Why fees are money: `protocol/src/block/v1.rs` credits the sum of every
//! packed transaction's `fee_got` to the block's fee receiver, which is the
//! coinbase address, which is the pool's own settlement wallet. A hold-back of
//! the subsidy alone therefore leaves the fee income sitting in the balance and
//! payable at zero confirmations. Orphan the block and that money is gone from
//! the chain while the payout that spent it is still valid - the operator funds
//! the difference out of their own pocket.
//!
//! This path has never run against a won block, so it is driven here with bytes
//! a real node produced. Every fixture under `fixtures/node/` was captured with
//! curl from a running full node (v1.0.10, database type 8) over its real HTTP
//! API; see `fixtures/node/README.md` for exactly how. The two multi-transaction
//! blocks were mined for the purpose, with DELIBERATELY DIFFERENT fees, so the
//! summation and the single round-up at the end are exercised by fees the node
//! itself serialized rather than by numbers someone typed.
//!
//! `block_fees` talks HTTP, so the fixtures are served back over HTTP by a stub
//! socket below. That keeps `get_json`, the per-transaction loop, the early
//! return and the arithmetic all inside the test instead of around it.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use hbit_pool::{
    BlockFees, block_fees, block_reward_units, distributable_units, fin_fine_ceil,
    fine_to_units_ceil, http_client,
};

// ---------------------------------------------------------------------------
// Fixtures: raw response bodies, byte for byte as the node sent them.
// include_str! so deleting a fixture breaks the build rather than the meaning.
// ---------------------------------------------------------------------------

/// Height 307: a real block with NO transactions. `"transaction":0` and an
/// EMPTY `tx_hash_list` - the node does emit the key, which is what lets the
/// pool tell "no transactions" apart from "the field is missing".
const BLOCK_307_0TX: &str = include_str!("fixtures/node/block_307_intro_0tx.json");

/// Height 309: a real block with THREE real transactions, fees 0.01 + 0.003 +
/// 0.0007 HAC. Sums to 13_700_000 fine steps: a seventh of a payout unit.
const BLOCK_309_3TX: &str = include_str!("fixtures/node/block_309_intro_3tx.json");
const TX_309_A: &str = include_str!("fixtures/node/tx_309_fee_1_246.json");
const TX_309_B: &str = include_str!("fixtures/node/tx_309_fee_3_245.json");
const TX_309_C: &str = include_str!("fixtures/node/tx_309_fee_7_244.json");

/// Height 308: a real block with TWO real transactions, fees 0.1 HAC and one
/// single fine step (1e-9 HAC). Sums to 100_000_001 steps - one step PAST a
/// payout-unit boundary, which is the case a truncating sum gets wrong.
const BLOCK_308_2TX: &str = include_str!("fixtures/node/block_308_intro_2tx.json");
const TX_308_A: &str = include_str!("fixtures/node/tx_308_fee_1_247.json");
const TX_308_B: &str = include_str!("fixtures/node/tx_308_fee_1_239.json");

/// The SAME real transaction as `TX_309_A`, re-fetched with `unit=mei`, where
/// the node renders `fee_got` as the decimal `"0.01"` instead of the
/// `"mantissa:unit"` the pool parses. A real, valid, node-produced body that
/// this pool cannot value.
const TX_309_A_MEI: &str = include_str!("fixtures/node/tx_309_fee_unit_mei.json");

/// The node's real error objects.
const BLOCK_MISSING_ERR: &str = include_str!("fixtures/node/block_missing_error.json");
const TX_MISSING_ERR: &str = include_str!("fixtures/node/tx_missing_error.json");
const TX_BAD_HASH_ERR: &str = include_str!("fixtures/node/tx_bad_hash_error.json");

/// The real body of a 404 from this node: completely empty, `content-length: 0`.
/// This is what a pool pointed at a node that does not serve the route gets.
const EMPTY_404_BODY: &str = include_str!("fixtures/node/unknown_route_body.txt");

/// Our block hashes, as the fixtures report them.
const HASH_307: &str = "000000e9238e6cf0783deb99af499e35987d61b6a36d022a0bd963dc0d161785";
const HASH_308: &str = "000004cc6e0eab13b4255d52615de57ffa2d9359b614ce2d36b4f58dd67e613e";
const HASH_309: &str = "000009bd2830d6e8172dd0d8d121597f456ae4ed4a4b62fffed1240700b71fd5";

const TXH_309_A: &str = "97a0553885ef4f1e52c28c9c3f87cbf60dfba48b7fb617fd77a7196c9f2810e4";
const TXH_309_B: &str = "3bcaa4a65088a1014dc01c516bce199f07204181d50ecc7a4e98f717de191dd2";
const TXH_309_C: &str = "c2a3f79809a2ebb0796bd427fa25a341bcd21b3ddc808d5024b18b5bfdbefddf";
const TXH_308_A: &str = "efa4c5fd8b0abd7aab12a72a1c0accff6ca6cfcae669c922e3deec7a1bfe9cb7";
const TXH_308_B: &str = "2ef06a4cff86bfb2bedd3faac2d6aac15ce5c50224d029d965ea71b882110019";

// ---------------------------------------------------------------------------
// A stub node: serves the fixture bytes over real HTTP on a loopback port.
// ---------------------------------------------------------------------------

struct StubNode {
    base: String,
    _thread: std::thread::JoinHandle<()>,
}

/// What to answer for one request.
#[derive(Clone)]
struct Reply {
    status: &'static str,
    body: String,
}

impl Reply {
    fn ok(body: &str) -> Self {
        Reply {
            status: "200 OK",
            body: body.to_string(),
        }
    }
    fn not_found(body: &str) -> Self {
        Reply {
            status: "404 Not Found",
            body: body.to_string(),
        }
    }
}

/// `intro` answers `/query/block/intro`; `txs` is keyed by the `hash=` value in
/// `/query/transaction`. A transaction the map does not know gets no reply at
/// all - the connection is closed mid-response, which is the "truncated body"
/// case a proxy or a dropped connection really produces.
fn stub_node(intro: Reply, txs: HashMap<&'static str, Reply>) -> StubNode {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub node");
    let port = listener.local_addr().expect("stub addr").port();
    let txs = Arc::new(txs);
    let thread = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let Some(target) = request_target(&mut stream) else {
                continue;
            };
            if target.contains("/query/block/intro") {
                write_reply(&mut stream, &intro);
                continue;
            }
            let hash = target
                .split("hash=")
                .nth(1)
                .map(|s| s.split('&').next().unwrap_or("").to_string())
                .unwrap_or_default();
            match txs.get(hash.as_str()) {
                Some(r) => write_reply(&mut stream, r),
                // Announce a body and then send nothing: the client sees the
                // connection close mid-body.
                None => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 4096\r\n\r\n{\"ret\":0,\"fee_g",
                    );
                }
            }
        }
    });
    StubNode {
        base: format!("http://127.0.0.1:{port}"),
        _thread: thread,
    }
}

fn request_target(stream: &mut TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    // Drain headers so the client is not left writing into a full buffer.
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
    line.split_whitespace().nth(1).map(|s| s.to_string())
}

fn write_reply(stream: &mut TcpStream, r: &Reply) {
    let _ = write!(
        stream,
        "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
        r.status,
        r.body.len(),
        r.body
    );
    let _ = stream.flush();
}

fn ask(intro: Reply, txs: Vec<(&'static str, Reply)>, height: u64, our_hash: &str) -> BlockFees {
    let node = stub_node(intro, txs.into_iter().collect());
    block_fees(&http_client(), &node.base, height, our_hash)
}

/// The figure the settlement path actually holds back for one immature block:
/// the coinbase subsidy PLUS whatever the block's transactions paid in fees.
/// `None` is "this block cannot be valued, settle nothing" - see
/// `count_immature_fees` in server.rs, which returns None on `Unknown`.
fn holdback_units(height: u64, fees: &BlockFees) -> Option<u64> {
    match fees {
        BlockFees::Counted(f) => Some(block_reward_units(height).saturating_add(*f)),
        BlockFees::NotOnChain => Some(0),
        BlockFees::Unknown(_) => None,
    }
}

// ---------------------------------------------------------------------------
// The block that carried nothing.
// ---------------------------------------------------------------------------

#[test]
fn a_real_block_with_no_transactions_holds_back_the_subsidy_and_no_more() {
    let got = ask(Reply::ok(BLOCK_307_0TX), vec![], 307, HASH_307);
    assert_eq!(got, BlockFees::Counted(0), "body: {BLOCK_307_0TX}");

    // 1 HAC of subsidy is 10 payout units of 0.1 HAC, and the fixture's own
    // "reward":"1:248" says the same thing.
    assert_eq!(block_reward_units(307), 10);
    assert_eq!(holdback_units(307, &got), Some(10));

    // An empty `tx_hash_list` really is what the node emits, and it is NOT the
    // same as the key being absent: see the missing-field test below.
    assert!(BLOCK_307_0TX.contains(r#""tx_hash_list":[]"#));
    assert!(BLOCK_307_0TX.contains(r#""transaction":0"#));
}

// ---------------------------------------------------------------------------
// The block that carried several.
// ---------------------------------------------------------------------------

#[test]
fn three_real_transaction_fees_are_summed_before_they_are_rounded_up_once() {
    let got = ask(
        Reply::ok(BLOCK_309_3TX),
        vec![
            (TXH_309_A, Reply::ok(TX_309_A)),
            (TXH_309_B, Reply::ok(TX_309_B)),
            (TXH_309_C, Reply::ok(TX_309_C)),
        ],
        309,
        HASH_309,
    );

    // 0.01 + 0.003 + 0.0007 HAC = 0.0137 HAC = 13_700_000 fine steps, which is
    // 0.137 of a payout unit. ONE unit held back, not three: rounding each fee
    // up on its own would freeze a whole unit per transaction for a whole
    // maturity window, and truncating instead of rounding would hold back
    // NOTHING and hand the fee income out at zero confirmations.
    assert_eq!(fin_fine_ceil("1:246"), Some(10_000_000));
    assert_eq!(fin_fine_ceil("3:245"), Some(3_000_000));
    assert_eq!(fin_fine_ceil("7:244"), Some(700_000));
    assert_eq!(fine_to_units_ceil(13_700_000), 1);

    assert_eq!(got, BlockFees::Counted(1));
    assert_eq!(holdback_units(309, &got), Some(10 + 1));

    // The node's own list is the coinbase-excluded one: three transactions in
    // the block, three hashes, and the coinbase hash is not among them. This is
    // the half of the reading path that cannot be checked by reading.
    assert!(BLOCK_309_3TX.contains(r#""transaction":3"#));
    for h in [TXH_309_A, TXH_309_B, TXH_309_C] {
        assert!(BLOCK_309_3TX.contains(h), "missing {h}");
    }
    assert_eq!(BLOCK_309_3TX.matches("\",\"").count() > 0, true);
}

#[test]
fn fees_one_fine_step_past_a_unit_boundary_hold_back_the_next_whole_unit() {
    let got = ask(
        Reply::ok(BLOCK_308_2TX),
        vec![
            (TXH_308_A, Reply::ok(TX_308_A)),
            (TXH_308_B, Reply::ok(TX_308_B)),
        ],
        308,
        HASH_308,
    );

    // 0.1 HAC is exactly one payout unit; the second fee is a single fine step
    // on top of it. 100_000_001 steps must hold back TWO units, because the
    // balance this is subtracted from is itself floored to whole units and that
    // floor can round the straddling unit up.
    assert_eq!(fin_fine_ceil("1:247"), Some(100_000_000));
    assert_eq!(fin_fine_ceil("1:239"), Some(1));
    assert_eq!(fine_to_units_ceil(100_000_001), 2);

    assert_eq!(got, BlockFees::Counted(2));
    assert_eq!(holdback_units(308, &got), Some(10 + 2));
}

// ---------------------------------------------------------------------------
// A fee this pool cannot value. NEVER zero.
// ---------------------------------------------------------------------------

#[test]
fn a_fee_rendered_in_an_unusual_unit_stops_settlement_instead_of_reading_as_zero() {
    // The node really does render this - it is the same transaction as
    // TX_309_A, asked for with unit=mei, and it answers ret=0 with a perfectly
    // valid body whose fee_got is the decimal "0.01".
    assert!(TX_309_A_MEI.contains(r#""fee_got":"0.01""#));
    assert!(TX_309_A_MEI.contains(r#""ret":0"#));
    assert_eq!(fin_fine_ceil("0.01"), None);

    let got = ask(
        Reply::ok(BLOCK_309_3TX),
        vec![
            (TXH_309_A, Reply::ok(TX_309_A_MEI)),
            (TXH_309_B, Reply::ok(TX_309_B)),
            (TXH_309_C, Reply::ok(TX_309_C)),
        ],
        309,
        HASH_309,
    );

    assert!(
        matches!(got, BlockFees::Unknown(_)),
        "a fee_got this pool cannot parse must stop settlement, got {got:?}"
    );
    assert_eq!(
        holdback_units(309, &got),
        None,
        "an unvaluable fee must settle NOTHING, never hold back zero fees"
    );
}

// ---------------------------------------------------------------------------
// A malformed body. NEVER zero.
// ---------------------------------------------------------------------------

#[test]
fn a_malformed_block_body_stops_settlement_instead_of_reading_as_zero() {
    // The real body of a 404 from this node: empty. `get_json` cannot parse it,
    // so it arrives as a bare JSON string rather than an object.
    assert_eq!(EMPTY_404_BODY, "");
    let got = ask(Reply::not_found(EMPTY_404_BODY), vec![], 309, HASH_309);
    assert!(matches!(got, BlockFees::Unknown(_)), "got {got:?}");
    assert_eq!(holdback_units(309, &got), None);

    // A real intro body cut short - what a dropped connection or a proxy buffer
    // limit produces. Valid JSON never resumes, so it must not be believed.
    let truncated = &BLOCK_309_3TX[..BLOCK_309_3TX.len() / 2];
    let got = ask(Reply::ok(truncated), vec![], 309, HASH_309);
    assert!(matches!(got, BlockFees::Unknown(_)), "got {got:?}");
    assert_eq!(holdback_units(309, &got), None);

    // The real intro for OUR block with `tx_hash_list` quietly dropped. This is
    // the dangerous one: ret=0, our hash, everything else intact. Reading it as
    // "the block had no transactions" would return Counted(0) on EVERY block
    // and release every block's fee income at zero confirmations.
    let no_list = BLOCK_309_3TX.replace(
        r#""tx_hash_list":["97a0553885ef4f1e52c28c9c3f87cbf60dfba48b7fb617fd77a7196c9f2810e4","3bcaa4a65088a1014dc01c516bce199f07204181d50ecc7a4e98f717de191dd2","c2a3f79809a2ebb0796bd427fa25a341bcd21b3ddc808d5024b18b5bfdbefddf"],"#,
        "",
    );
    assert!(!no_list.contains("tx_hash_list"), "fixture text drifted");
    assert!(no_list.contains(HASH_309) && no_list.contains(r#""ret":0"#));
    let got = ask(Reply::ok(&no_list), vec![], 309, HASH_309);
    assert!(
        matches!(got, BlockFees::Unknown(_)),
        "a missing tx_hash_list must never read as an empty one, got {got:?}"
    );
    assert_eq!(holdback_units(309, &got), None);

    // A transaction body cut short mid-JSON: the stub answers this way for any
    // hash it does not know.
    let got = ask(Reply::ok(BLOCK_309_3TX), vec![], 309, HASH_309);
    assert!(matches!(got, BlockFees::Unknown(_)), "got {got:?}");
    assert_eq!(holdback_units(309, &got), None);
}

// ---------------------------------------------------------------------------
// The node answers an error object.
// ---------------------------------------------------------------------------

#[test]
fn the_nodes_own_error_objects_are_read_for_what_they_actually_say() {
    // "cannot find block" is a definitive answer: the chain holds nothing at
    // that height, so it credited us nothing and there is nothing to hold back.
    assert!(BLOCK_MISSING_ERR.contains(r#""ret":1"#));
    assert!(BLOCK_MISSING_ERR.contains("cannot find block"));
    let got = ask(Reply::ok(BLOCK_MISSING_ERR), vec![], 999_999, HASH_309);
    assert_eq!(got, BlockFees::NotOnChain);
    assert_eq!(holdback_units(999_999, &got), Some(0));

    // The chain holds a block at our height, but it is not ours - another block
    // won it. Also nothing credited. (Real 307 body, asked about as if it were
    // our block.)
    let got = ask(Reply::ok(BLOCK_307_0TX), vec![], 307, HASH_309);
    assert_eq!(got, BlockFees::NotOnChain);

    // But an error on a transaction the node ITSELF just listed in our block is
    // not a zero fee. That transaction paid something; we simply cannot say how
    // much, and the fee is already in the wallet.
    assert!(TX_MISSING_ERR.contains(r#""ret":1"#));
    let got = ask(
        Reply::ok(BLOCK_309_3TX),
        vec![
            (TXH_309_A, Reply::ok(TX_309_A)),
            (TXH_309_B, Reply::ok(TX_MISSING_ERR)),
            (TXH_309_C, Reply::ok(TX_309_C)),
        ],
        309,
        HASH_309,
    );
    assert!(matches!(got, BlockFees::Unknown(_)), "got {got:?}");
    assert_eq!(holdback_units(309, &got), None);

    // Same for the node's other error shape.
    assert!(TX_BAD_HASH_ERR.contains("transaction hash format invalid"));
    let got = ask(
        Reply::ok(BLOCK_309_3TX),
        vec![
            (TXH_309_A, Reply::ok(TX_BAD_HASH_ERR)),
            (TXH_309_B, Reply::ok(TX_309_B)),
            (TXH_309_C, Reply::ok(TX_309_C)),
        ],
        309,
        HASH_309,
    );
    assert!(matches!(got, BlockFees::Unknown(_)), "got {got:?}");
}

// ---------------------------------------------------------------------------
// No node at all.
// ---------------------------------------------------------------------------

#[test]
fn an_unreachable_node_stops_settlement_instead_of_reading_as_zero() {
    // Bind a port, learn it, then drop the listener: nothing is listening.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let base = format!("http://127.0.0.1:{port}");
    let got = block_fees(&http_client(), &base, 309, HASH_309);
    assert!(matches!(got, BlockFees::Unknown(_)), "got {got:?}");
    assert_eq!(holdback_units(309, &got), None);
}

// ---------------------------------------------------------------------------
// What the hold-back is FOR.
// ---------------------------------------------------------------------------

#[test]
fn the_fee_half_of_the_holdback_is_what_keeps_it_from_being_paid_out() {
    // A wallet holding exactly one block's income: 1 HAC of subsidy (10 units)
    // plus its fees. Counting the subsidy alone leaves the fee units payable.
    let fees = ask(
        Reply::ok(BLOCK_308_2TX),
        vec![
            (TXH_308_A, Reply::ok(TX_308_A)),
            (TXH_308_B, Reply::ok(TX_308_B)),
        ],
        308,
        HASH_308,
    );
    assert_eq!(fees, BlockFees::Counted(2));

    let reserve = 0;
    let balance = 12; // 10 units of subsidy + 2 units of fee income, all immature

    // Subsidy alone: two units of fee income are distributable while the block
    // can still be orphaned. That is the bug this hold-back exists to close.
    assert_eq!(
        distributable_units(balance, block_reward_units(308), reserve),
        Some(2)
    );

    // Subsidy plus fees: nothing is payable until the block matures.
    assert_eq!(
        distributable_units(balance, holdback_units(308, &fees).expect("counted"), reserve),
        None
    );
}
