//! Does the pool refuse to start with empty accounting beside a funded wallet?
//!
//! This is the failure the whole B1 fix exists to stop. Before it, a state file
//! the pool could not read - a permission change, a half-written file, a
//! restored backup with the wrong owner - left the server running with zero
//! owed, zero paid, zero in flight. The next settlement then distributed the
//! whole wallet balance to whoever was in the share window: every debt
//! forgotten, every payout already on the wire re-signed, every maturing block
//! paid at zero confirmations.
//!
//! It drives the REAL hbit-pool-server binary against a stub mainnet node, with
//! a real wallet file and a corrupt ledger beside it, and requires the process
//! to refuse rather than serve. The classify decision is unit-tested in lib.rs;
//! this proves the decision is actually WIRED into startup, ahead of anything
//! that moves money.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;

/// The mainnet genesis hash, from the node's own constant via the crate.
fn genesis_hex() -> String {
    hbit_pool::mainnet_genesis_hex()
}

struct Stub {
    base: String,
    _thread: std::thread::JoinHandle<()>,
}

/// The least a node must answer for `verify_chain_params` to pass on an EMPTY
/// chain: its tip is height 0, and block 0 is the mainnet genesis. That is
/// enough to reach the ledger gate, which is all this test is about - it is not
/// a mining test.
fn stub_empty_mainnet() -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let g = genesis_hex();
    let thread = std::thread::spawn(move || {
        for s in listener.incoming() {
            let Ok(mut s) = s else { break };
            let Some(target) = request_target(&mut s) else {
                continue;
            };
            let body = if target.starts_with("/query/latest") {
                r#"{"ret":0,"height":0,"diamond":0}"#.to_string()
            } else if target.starts_with("/query/block/intro") {
                format!(
                    r#"{{"ret":0,"hash":"{g}","height":0,"timestamp":1549250700,"difficulty":1}}"#
                )
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

fn server_binary() -> std::path::PathBuf {
    let mut p = std::env::current_exe().expect("test exe");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join(format!("hbit-pool-server{}", std::env::consts::EXE_SUFFIX))
}

#[test]
fn a_corrupt_ledger_beside_a_wallet_refuses_to_start_and_does_not_touch_the_file() {
    let bin = server_binary();
    assert!(
        bin.exists(),
        "{} was not built; this test proves nothing without it",
        bin.display()
    );

    let dir = std::env::temp_dir().join(format!("hbit-b1-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");

    // A real wallet file, made the way the server would make it, so the refusal
    // is unambiguously about the LEDGER and not a missing wallet.
    let wallet = dir.join("pool-wallet.key");
    let _acc = hbit_pool::load_or_create_wallet(&wallet.to_string_lossy());

    // A ledger the pool cannot read: a half-written JSON object. This is exactly
    // what a crash mid-write leaves, and it used to be swallowed.
    let state = dir.join("pool-wallet.key.state.json");
    let corrupt = r#"{"schema":1,"owed":[["addr",100],"#;
    std::fs::write(&state, corrupt).expect("write corrupt ledger");

    let node = stub_empty_mainnet();

    let out = Command::new(&bin)
        .arg(&node.base) // <node>
        .arg(&wallet) // <wallet_file>
        .arg("127.0.0.1:0") // <listen> - an ephemeral port; we never serve
        .arg("24") // <share_bits>
        .arg("mainnet") // <chain>
        .output()
        .expect("run hbit-pool-server");

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !out.status.success(),
        "the server must EXIT rather than serve on an unreadable ledger. Output:\n{text}"
    );
    assert!(
        text.contains("REFUSING to start"),
        "the operator has to be told plainly why. Output:\n{text}"
    );
    assert!(
        text.contains("empty accounting") || text.contains("pay the current"),
        "and told the money stake, not just that a file was bad. Output:\n{text}"
    );

    // The file must be untouched: it is the only copy of the accounting, and a
    // refusal that mangled it would be worse than the bug it replaced.
    let after = std::fs::read_to_string(&state).expect("the ledger file must still be there");
    assert_eq!(
        after, corrupt,
        "the refusal must not alter the accounting file"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_wallet_with_a_readable_empty_ledger_is_not_refused_for_that() {
    // The other side of the gate: a genuinely fresh, well-formed ledger must NOT
    // trip the ledger refusal. This minimal stub is an EMPTY chain, so the
    // server will exit shortly afterward for an unrelated reason (it cannot
    // build a template to mine on genesis alone). That is fine: the claim here
    // is narrow and precise - the ledger gate did not fire - so it checks the
    // output text rather than the exit code, which the corrupt-ledger test
    // above already owns.
    let bin = server_binary();
    assert!(bin.exists(), "{} not built", bin.display());

    let dir = std::env::temp_dir().join(format!("hbit-b1-ok-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let wallet = dir.join("pool-wallet.key");
    let _acc = hbit_pool::load_or_create_wallet(&wallet.to_string_lossy());
    // A well-formed, empty-but-valid ledger.
    std::fs::write(
        dir.join("pool-wallet.key.state.json"),
        r#"{"schema":1,"window":4096,"order":[],"banked":[],"owed":[],"immature":[],"payouts_inflight":[],"settle_pending_txs":[]}"#,
    )
    .expect("write ledger");

    let node = stub_empty_mainnet();
    let out = Command::new(&bin)
        .arg(&node.base)
        .arg(&wallet)
        .arg("127.0.0.1:0")
        .arg("24")
        .arg("mainnet")
        .output()
        .expect("run hbit-pool-server");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !text.contains("empty accounting") && !text.contains("pay the current share window"),
        "a valid empty ledger beside a wallet must not trip the ledger refusal; the pool has \
         to be able to start for the first time. Output:\n{text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
