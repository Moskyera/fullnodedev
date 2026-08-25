//! Can anything on the network choose who the manual settler pays?
//!
//! It used to be able to. `hbit-pool-payout` derived its entire recipient list
//! from an unauthenticated plain-HTTP GET of `{pool_base}/stats`, where
//! pool_base is argv[1], and read the pool's own accounting file only when that
//! answer parsed to zero rows. Whatever replied on that URL therefore chose
//! every recipient of the whole distributable balance, in transactions signed
//! with the pool wallet key.
//!
//! The endpoint could not even do the job it was there for: the tool holds the
//! exclusive settlement lock for its entire run, so the pool server is by
//! construction not running while it works. Anything that answers is, on the
//! balance of it, not the pool.
//!
//! This test runs the REAL binary against a stub that answers `/stats` with an
//! address the ledger has never heard of, and requires that address never to
//! reach the plan.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;

struct Stub {
    base: String,
    _thread: std::thread::JoinHandle<()>,
}

/// A node that answers everything the tool needs to reach the split, and a
/// `/stats` that tries to name the recipients.
///
/// `attacker` must be a REAL, payable address. The first version of this test
/// used a made-up string, the payable filter dropped it before the split, and
/// the test passed against the vulnerable code: it proved nothing. An attacker
/// would of course supply an address they can spend from.
fn stub(balance_hac: &'static str, attacker: String) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let thread = std::thread::spawn(move || {
        for s in listener.incoming() {
            let Ok(mut s) = s else { break };
            let Some(target) = request_target(&mut s) else {
                continue;
            };
            let body = if target.starts_with("/query/latest") {
                r#"{"ret":0,"height":800000,"diamond":0}"#.to_string()
            } else if target.starts_with("/query/balance") {
                format!(r#"{{"ret":0,"list":[{{"hacash":"{balance_hac}"}}]}}"#)
            } else if target.starts_with("/stats") {
                // The whole attack, in one line: a credit table naming an
                // address the pool has never credited.
                format!(r#"{{"credit":[["{attacker}",1000000]]}}"#)
            } else {
                r#"{"ret":1,"errmsg":"stub refuses everything else"}"#.to_string()
            };
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.flush();
        }
    });
    Stub {
        base,
        _thread: thread,
    }
}

fn request_target(s: &mut TcpStream) -> Option<String> {
    let mut r = BufReader::new(s.try_clone().ok()?);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).ok()? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
    line.split_whitespace().nth(1).map(|s| s.to_string())
}

/// A state file whose share window credits exactly one address.
fn write_ledger(state_file: &std::path::Path, honest: &str) {
    // `order` is the share window as (worker, arrival ms). One worker only, so
    // whatever the split is it can name exactly one address.
    //
    // TWO shares, not one. Credit is residence in the window, measured against
    // an anchor the file itself carries: the last moment the pool that wrote it
    // was accounting, which is the newest arrival time in here. With a single
    // share the anchor IS that share's arrival, its residence is zero, and the
    // tool correctly reports nothing to pay. The older share below has a full
    // horizon of residence.
    let body = format!(
        r#"{{"window":4096,"credit_horizon_ms":600000,
             "order":[["{honest}",1000],["{honest}",601000]],"banked":[],
             "accepted":1,"blocks":0,"orphaned":0,
             "settle_pending_txs":[],"payouts_inflight":[],"owed":[],
             "immature":[]}}"#
    );
    std::fs::write(state_file, body).expect("write ledger");
}

fn payout_binary() -> std::path::PathBuf {
    // target/debug/deps/<this test>, so the binary is two levels up.
    let mut p = std::env::current_exe().expect("test exe");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join(format!("hbit-pool-payout{}", std::env::consts::EXE_SUFFIX))
}

#[test]
fn a_stats_endpoint_cannot_choose_who_gets_paid() {
    let bin = payout_binary();
    if !bin.exists() {
        // cargo test builds every bin in this package before running tests, so
        // this should not happen. Fail rather than pass quietly: a test that
        // skips itself on the money path is worse than no test.
        panic!(
            "{} was not built; this test proves nothing without it",
            bin.display()
        );
    }

    let dir = std::env::temp_dir().join(format!("hbit-payout-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    // Three DISTINCT addresses, and the distinctness is the point.
    //
    // An earlier version of this test credited the pool's own wallet in the
    // ledger. The tool prints "wallet = <address>" early in every run, so the
    // "a plan was printed" assertion was satisfied by that line and the test
    // passed without a plan ever existing. A miner is not the pool.
    let wallet = dir.join("pool-wallet.key");
    let _pool_acc = hbit_pool::load_or_create_wallet(&wallet.to_string_lossy());

    let honest_acc = hbit_pool::load_or_create_wallet(&dir.join("miner.key").to_string_lossy());
    let honest = honest_acc.readable().to_string();
    write_ledger(&dir.join("pool-wallet.key.state.json"), &honest);

    // A second real wallet, standing in for an address the attacker controls.
    // It is a valid payable address, so nothing but the fix itself can keep it
    // out of the plan.
    let attacker_acc =
        hbit_pool::load_or_create_wallet(&dir.join("attacker.key").to_string_lossy());
    let attacker = attacker_acc.readable().to_string();
    assert_ne!(attacker, honest, "the two wallets must differ");

    // A balance big enough that a split really happens.
    let node = stub("12:248", attacker.clone());

    // No --commit: a dry run that prints the plan and signs nothing.
    let out = Command::new(&bin)
        .arg(&node.base) // <pool_base>, the URL that used to decide everything
        .arg(&node.base) // <node>
        .arg("mainnet")
        .arg(&wallet)
        .output()
        .expect("run hbit-pool-payout");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !text.contains(&attacker),
        "the /stats answer named {attacker} as a recipient and the tool must never have \
         looked at it. Output was:\n{text}"
    );
    // And the run must have got far enough for that to mean something: a tool
    // that exited before the split would pass the assertion above for the wrong
    // reason.
    // The run must have produced a real plan naming a real recipient. Without
    // this the assertion above passes for the wrong reason on any build that
    // exits early, which is exactly how the first version of this test passed
    // against the vulnerable code.
    assert!(
        text.contains(&honest),
        "the tool never printed a plan naming the ledger's own address, so it did not reach \
         the point where recipients are chosen and this test proves nothing. Output was:\n{text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
