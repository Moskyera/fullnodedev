//! Does this pool know which chain its node is on?
//!
//! Every other startup check asks the node about its own tip and verifies the
//! answer is self-consistent: the tip's difficulty really does follow from the
//! block before it. A node on a different chain passes all of them effortlessly,
//! because it is perfectly consistent with itself.
//!
//! The failure that costs money is not exotic. A node stalled part-way through a
//! sync, or pointed at a private chain, answers every question happily. The pool
//! then mines a chain nobody else is on, watches its own blocks get buried
//! sixteen deep THERE, releases the coinbase hold-back on that evidence and
//! signs real payouts out of a real wallet against income the real chain never
//! credited. Miners burn real power for shares that can never mature.
//!
//! These tests drive `verify_chain_params` against a stub node over real HTTP.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use hbit_pool::difficulty::ChainParams;
use hbit_pool::{http_client, mainnet_genesis_hex, verify_chain_params};

/// The mainnet genesis hash, written out once.
///
/// The pool derives this from `mint::genesis` and never from a literal, so this
/// literal exists only to catch the constant itself moving. The same value is
/// proved from the genesis header bytes in x16rs-cuda/tests/genesis_vector.rs,
/// where it is the output of hashing the block rather than an assertion about it.
const MAINNET_GENESIS: &str = "000000077790ba2fcdeaef4a4299d9b667135bac577ce204dee8388f1b97f7e6";

#[test]
fn the_pool_and_the_node_agree_on_what_mainnet_block_zero_is() {
    assert_eq!(
        mainnet_genesis_hex(),
        MAINNET_GENESIS,
        "the canonical genesis hash moved. Either mainnet changed, which it did not, or \
         something edited the constant every payout in this pool is anchored to"
    );
}

struct StubNode {
    base: String,
    _thread: std::thread::JoinHandle<()>,
}

/// A node that answers `/query/latest` with `tip`, and `/query/block/intro` with
/// whatever `intro_for` returns for the requested height.
fn stub_node(
    tip: u64,
    intro_for: impl Fn(u64) -> Option<String> + Send + Sync + 'static,
) -> StubNode {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub node");
    let port = listener.local_addr().expect("stub addr").port();
    let intro_for = Arc::new(intro_for);
    let thread = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let Some(target) = request_target(&mut stream) else {
                continue;
            };
            let body = if target.contains("/query/latest") {
                Some(format!(
                    r#"{{"ret":0,"list":[{{"height":{tip},"diamond":0}}]}}"#
                ))
            } else if target.contains("/query/block/intro") {
                let h = target
                    .split("height=")
                    .nth(1)
                    .and_then(|s| s.split('&').next())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(u64::MAX);
                intro_for(h)
            } else {
                None
            };
            match body {
                Some(b) => write_json(&mut stream, &b),
                None => write_json(&mut stream, r#"{"ret":1,"err":"not found"}"#),
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
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
    line.split_whitespace().nth(1).map(|s| s.to_string())
}

fn write_json(stream: &mut TcpStream, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
        body.len(),
        body
    );
}

/// A BLOCK 1 answer whose `prevhash` is `genesis`.
///
/// Block 1, not block 0, because that is what a real node serves. Verified
/// against a live mainnet node at height 771593: `?height=0` answers
/// "cannot find block" and so does a lookup by the genesis hash, because the
/// handler defaults `height` to 0 and cannot tell zero from "not given". Block
/// 1's prevhash IS the genesis hash by construction.
fn genesis_reply(genesis: &str) -> String {
    format!(
        r#"{{"ret":0,"hash":"001e231cb03f9938d54f04407797b8188f0375eb10f0bcb426dccae87dcadb56","prevhash":"{genesis}","height":1,"timestamp":1549250864,"difficulty":4294967294}}"#
    )
}

#[test]
fn a_node_on_another_chain_is_refused_by_its_genesis() {
    // A private chain, a testnet, a node someone else's pool is running: all of
    // them answer every other check correctly and only differ here.
    let node = stub_node(800_000, |h| {
        (h == 1)
            .then(|| genesis_reply("dead00000000000000000000000000000000000000000000000000000beef"))
    });
    let err = verify_chain_params(&http_client(), &node.base, &ChainParams::mainnet())
        .expect_err("a foreign genesis must stop this pool before it credits a single share");
    assert!(
        err.contains("NOT on the chain"),
        "the operator has to be told what is wrong, not just that something is: {err}"
    );
    assert!(
        err.contains(MAINNET_GENESIS),
        "and has to be shown what was expected: {err}"
    );
}

#[test]
fn a_node_with_no_chain_at_all_cannot_identify_itself_and_is_refused() {
    // A tip of 0 used to return Ok unconditionally, on the reasoning that an
    // empty chain has stored nothing to compare to. A node with no blocks
    // genuinely cannot be identified - this one does not even serve its own
    // genesis - so the answer is to refuse, not to wave it through. A mainnet
    // node has 700000 blocks or more.
    let node = stub_node(0, |_| None);
    let err = verify_chain_params(&http_client(), &node.base, &ChainParams::mainnet())
        .expect_err("a node with no chain cannot prove which chain it is");
    assert!(err.contains("no blocks at all"), "{err}");
}

#[test]
fn a_chain_that_begins_somewhere_else_is_refused() {
    let node = stub_node(5, |h| {
        (h == 1).then(|| {
            genesis_reply("00000000000000000000000000000000000000000000000000000000000000ff")
        })
    });
    let err = verify_chain_params(&http_client(), &node.base, &ChainParams::mainnet())
        .expect_err("a chain beginning elsewhere is not mainnet");
    assert!(err.contains("NOT on the chain"), "{err}");
}

#[test]
fn the_genesis_hash_is_matched_without_caring_about_hex_case() {
    let node = stub_node(800_000, |h| match h {
        1 => Some(genesis_reply(&MAINNET_GENESIS.to_uppercase())),
        800_000 => Some(block_reply(800_000, now_unix() - 60)),
        other => Some(block_reply(other, 1_600_000_000)),
    });
    match verify_chain_params(&http_client(), &node.base, &ChainParams::mainnet()) {
        Ok(()) => {}
        Err(e) => assert!(
            !e.contains("NOT on the chain"),
            "hex case is a rendering choice, not a different chain: {e}"
        ),
    }
}

#[test]
fn a_node_that_cannot_produce_block_one_is_refused_rather_than_assumed_good() {
    // Unknown is not permission. A node that will not answer for block 1 has
    // not identified itself, and this pool is about to hold other people's money
    // on the strength of that identification.
    let node = stub_node(800_000, |_| None);
    let err = verify_chain_params(&http_client(), &node.base, &ChainParams::mainnet())
        .expect_err("no answer must not read as the right answer");
    assert!(
        err.contains("could not read block 1"),
        "the reason has to name what was missing: {err}"
    );
}

/// An intro reply for a block at `h`, stamped `ts`.
fn block_reply(h: u64, ts: u64) -> String {
    format!(r#"{{"ret":0,"hash":"{h:064x}","height":{h},"timestamp":{ts},"difficulty":520093695}}"#)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

#[test]
fn a_node_stalled_on_the_right_chain_is_refused_at_startup() {
    // The identity check proves the node knows what mainnet is. It says nothing
    // about whether the node is anywhere near the end of it, and a node stalled
    // part-way through a sync answers everything so far with total confidence.
    // This is the documented failure of this deployment: history sync finishes
    // short of the tip and then ignores live blocks until it is restarted.
    let stale = now_unix() - 4 * 3600;
    let node = stub_node(800_000, move |h| match h {
        1 => Some(genesis_reply(MAINNET_GENESIS)),
        800_000 => Some(block_reply(800_000, stale)),
        other => Some(block_reply(other, 1_600_000_000)),
    });
    let err = verify_chain_params(&http_client(), &node.base, &ChainParams::mainnet())
        .expect_err("a pool must not start against a node that stopped following the chain");
    assert!(
        err.contains("silence"),
        "the operator has to be told the chain has gone quiet, not just that something \
         failed: {err}"
    );
    assert!(
        err.contains("start the pool again"),
        "and what to do about it: {err}"
    );
}

#[test]
fn a_node_at_a_live_tip_gets_past_the_staleness_gate() {
    // The other side of the door. This node is refused later, on the difficulty
    // rule, because the stub is not a real chain - but it must not be refused
    // for being stale, or the gate would be refusing everyone.
    let node = stub_node(800_000, move |h| match h {
        1 => Some(genesis_reply(MAINNET_GENESIS)),
        800_000 => Some(block_reply(800_000, now_unix() - 60)),
        other => Some(block_reply(other, 1_600_000_000)),
    });
    match verify_chain_params(&http_client(), &node.base, &ChainParams::mainnet()) {
        Ok(()) => {}
        Err(e) => assert!(
            !e.contains("silence"),
            "a tip one minute old is a live chain: {e}"
        ),
    }
}

#[test]
fn a_testnet_is_not_checked_against_a_genesis_it_cannot_have() {
    // Documented trade-off. A testnet's genesis depends on whoever started that
    // chain, so there is nothing to verify against and pretending otherwise
    // would refuse every legitimate testnet. The identity guarantee in this file
    // is a MAINNET guarantee, and the pool's own production posture is mainnet.
    let node = stub_node(0, |h| {
        (h == 0).then(|| {
            genesis_reply("00000000000000000000000000000000000000000000000000000000000000ff")
        })
    });
    verify_chain_params(&http_client(), &node.base, &ChainParams::testnet(10, 10))
        .expect("a testnet genesis is whatever its operator made it");
}

/// The operator runbook that ships in the release archive, checked as text.
///
/// A doc claim about a refusal is a safety claim. An operator who believes the
/// pool refuses to start on a syncing node stops watching the node's own sync,
/// and this deployment's known failure is a history sync that finishes short of
/// the tip and then ignores live blocks. The runbook said exactly that for as
/// long as nothing was checking it, because a sentence in a markdown file is
/// the one part of this pool the compiler never reads.
#[test]
fn the_runbook_does_not_promise_a_syncing_refusal_the_pool_cannot_make() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repo root")
        .join("docs/POOL-OPERATOR.md");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    // Flattened, so re-wrapping a paragraph can neither hide a claim from this
    // test nor fake one into it.
    let doc = raw.split_whitespace().collect::<Vec<_>>().join(" ");

    // There is no such check and there cannot be one here: the node's
    // /query/latest answers with a height and a diamond number, and everything
    // this file drives - the genesis hash and the tip timestamp - is evidence
    // about the chain, never about the node's own sync state.
    assert!(
        !doc.contains("A node that is still syncing"),
        "the runbook promises a refusal on a syncing node. No such check exists, \
         and the node's API cannot report sync state at all"
    );

    // What the pool really refuses and halts on, each named where an operator
    // will look for it.
    for claim in [
        "block 1",
        "3600 seconds",
        "7200 seconds",
        "/query/latest",
        "cannot detect a syncing node",
        "hbit-v2/MAINNET-SAFETY.md",
    ] {
        assert!(
            doc.contains(claim),
            "the runbook has to say what the pool really does about the node, and \
             `{claim}` is missing from it"
        );
    }

    // The link above has to resolve in the archive as well as in the repository,
    // and the archive is flat: POOL-OPERATOR.md sits at its root beside the
    // copied directory. So the copy has to keep this name.
    let workflow = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repo root")
        .join(".github/workflows/release-pool.yml");
    let packaging = std::fs::read_to_string(&workflow).expect("read release-pool.yml");
    assert!(
        packaging.contains("cp -r docs/hbit-v2 \"$pooldir/hbit-v2\""),
        "the runbook links to hbit-v2/MAINNET-SAFETY.md, so the release archive has to \
         copy that directory under exactly that name or the link is dead for every \
         operator who reads the shipped copy"
    );
}
