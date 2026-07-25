//! Test miner for the pool protocol: pulls work, mines the 32-bit block nonce
//! against the pool's share target, submits shares. This is the worker side
//! that the real poworker will later speak.
//!
//! On `mkrl_modify_list`: this worker deliberately never rebuilds the merkle
//! root. `/work` hands it the FULL 89-byte header the pool itself reconstructs,
//! merkle root already folded, and the only bytes touched here are the nonce at
//! 79..83. That is what keeps it correct now that a pool block carries the
//! node's transactions - the sibling list only matters to a worker that computes
//! the root itself, which is the standard `/query/miner/pending` protocol, not
//! this one. Change that and this file must honour the list too.
//!
//! Usage: test-miner [pool_base] [worker_address] [shares_to_find]
//!   The worker id must be a payable HAC address: the pool credits shares under
//!   that key and refuses one it could never pay, exactly as on the paid path.

use pool_spike::pool_core;
use pool_spike::{find_str, find_u64, get_json, http_client};

/// Consecutive rejected submissions before this miner stops and says why.
/// Mining on regardless is how a worker burns hours producing nothing: the pool
/// rebuilds every share's header itself, so a worker hashing a different header
/// has EVERY share rejected, forever, however long it keeps trying.
const MAX_REJECTS: u64 = 8;

/// How many nonces one pass over a template probes before refetching work.
const SCAN_SPAN: u32 = 3_000_000;

/// Consecutive failed work fetches before giving up. A pool restart, a redeploy
/// or a moment of packet loss must not end a worker that is otherwise healthy:
/// exiting on the FIRST transport error is how a soak rig quietly loses half its
/// miners overnight and nobody notices until the share split looks wrong.
const MAX_WORK_FAILURES: u32 = 60;
const WORK_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// Accepted shares are logged in full for the first few, so a run visibly starts
/// working, and then only every Nth. See the call site for why.
const ACCEPT_LOG_FIRST: u64 = 5;
const ACCEPT_LOG_EVERY: u64 = 500;

/// Did the pool credit this submission? The pool answers `{"ok":true,...}` for a
/// credited share or block and `{"ok":false,"kind":...}` for anything else, so
/// counting a submission as "found" without reading `ok` reports work that was
/// never credited - which is exactly what this miner used to print.
fn accepted(resp: &serde_json::Value) -> bool {
    resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Did the pool answer at all? `get_json` reports a transport failure as
/// `{"http_error": ...}`, which carries no `ok` and so looks exactly like a
/// rejection to any check that only reads `ok`.
///
/// Keeping the two apart matters: a rejection streak means this worker is
/// hashing a header the pool does not share and every future share is worthless,
/// which is worth stopping for. A run of connection errors means the pool is
/// busy or restarting and the right move is to wait. Counting the second as the
/// first killed a healthy worker and told its operator, with confidence, that
/// its merkle root was wrong.
fn no_answer(resp: &serde_json::Value) -> bool {
    resp.get("http_error").is_some()
}

fn reject_kind(resp: &serde_json::Value) -> String {
    let kind = resp
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    match resp.get("err").and_then(|v| v.as_str()) {
        Some(err) => format!("{kind} ({err})"),
        None => kind.to_string(),
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let pool = a
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:9777".to_string());
    let pool = pool.trim_end_matches('/').to_string();
    let worker = a.get(2).cloned().unwrap_or_else(|| "w1".to_string());
    let want: u64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(3);

    let client = http_client();
    println!("== test-miner {worker} -> {pool} (want {want} shares) ==");

    let mut found = 0u64;
    let mut rejected = 0u64;
    let mut streak = 0u64;
    // Where the next pass starts hashing. Restarting every pass at 0 meant a
    // second pass over the same template re-found the SAME nonce and submitted
    // it again, which the pool answers "duplicate" forever: at an easy share
    // target this worker produced one share and then spun, submitting it
    // thousands of times. Carrying the cursor makes each pass explore new nonces.
    let mut scan_from = 0u32;
    let mut work_failures = 0u32;
    let mut submit_failures = 0u32;
    while found < want {
        let w = get_json(&client, &format!("{pool}/work?worker={worker}"));
        let (Some(height), Some(intro_hex), Some(st_hex)) = (
            find_u64(&w, "height"),
            find_str(&w, "intro"),
            find_str(&w, "share_target"),
        ) else {
            work_failures += 1;
            if work_failures >= MAX_WORK_FAILURES {
                println!("bad work response {work_failures} times, giving up: {w}");
                break;
            }
            println!("bad work response ({work_failures}/{MAX_WORK_FAILURES}), retrying: {w}");
            std::thread::sleep(WORK_RETRY);
            continue;
        };
        work_failures = 0;

        let mut intro = hex::decode(&intro_hex).expect("intro hex");
        if intro.len() != 89 {
            println!("unexpected header length {}", intro.len());
            break;
        }
        let stv = hex::decode(&st_hex).expect("share target hex");
        let mut share_target = [0u8; 32];
        share_target.copy_from_slice(&stv);

        // Mine: the block nonce lives at header bytes 79..83 (big-endian u32).
        // Nothing else in the header is touched, so the merkle root the pool
        // folded from its own coinbase and the node's transactions stays intact.
        let mut hit = None;
        let mut nonce = scan_from;
        for _ in 0..SCAN_SPAN {
            intro[79..83].copy_from_slice(&nonce.to_be_bytes());
            if pool_core::meets_target(height, &intro, &share_target) {
                hit = Some(nonce);
                break;
            }
            nonce = nonce.wrapping_add(1);
        }
        // Resume past whatever this pass reached, hit or miss, so the next pass
        // never re-offers a nonce this one already submitted.
        scan_from = nonce.wrapping_add(1);

        match hit {
            Some(nonce) => {
                let r = get_json(
                    &client,
                    &format!("{pool}/share?worker={worker}&height={height}&nonce={nonce}"),
                );
                // Only a submission the pool CREDITED counts. Counting every
                // submission made this miner report shares it was never paid for.
                if accepted(&r) {
                    found += 1;
                    streak = 0;
                    submit_failures = 0;
                    // One line per accepted share is fine when a share is real
                    // work. Against an easy share target it is not: a soak rig
                    // measured this writing 1 MB/s, rotating a 300 MB log every
                    // five minutes, which is tens of gigabytes of pointless disk
                    // wear a day. Rejections and faults still print every time,
                    // because those are the ones an operator needs to see.
                    if found <= ACCEPT_LOG_FIRST || found % ACCEPT_LOG_EVERY == 0 {
                        println!("height={height} nonce={nonce} accepted (total {found}) -> {r}");
                    }
                    continue;
                }
                println!("height={height} nonce={nonce} -> {r}");
                if no_answer(&r) {
                    // The pool said nothing, so it did not reject anything. Wait
                    // and keep the rejection streak untouched.
                    submit_failures += 1;
                    if submit_failures >= MAX_WORK_FAILURES {
                        eprintln!(
                            "the pool has not answered {submit_failures} submissions in a row; \
                             it looks down rather than unhappy with this worker. Stopping."
                        );
                        break;
                    }
                    std::thread::sleep(WORK_RETRY);
                    continue;
                }
                submit_failures = 0;
                rejected += 1;
                streak += 1;
                if streak >= MAX_REJECTS {
                    eprintln!(
                        "the pool rejected {streak} submissions in a row (last: {}). It rebuilds \
                         every share's header itself, so this worker is hashing a DIFFERENT \
                         header than the pool and nothing it submits can ever be credited. \
                         Stopping rather than mining for nothing.",
                        reject_kind(&r)
                    );
                    break;
                }
            }
            None => println!("no share found in range at height {height}; refetching work"),
        }
    }
    println!("done: {found} share(s) credited to {worker}, {rejected} rejected");
    if found < want {
        // A non-zero exit so a script driving this miner cannot mistake a run
        // that was rejected wholesale for a successful one.
        std::process::exit(1);
    }
}
