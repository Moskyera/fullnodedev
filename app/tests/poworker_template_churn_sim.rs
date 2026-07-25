//! The fullnode re-serializes `block_intro` on EVERY `/query/miner/pending`
//! request: it increments the served coinbase nonce, recomputes the merkle root
//! from it and re-serializes the intro, and the merkle root sits inside the intro.
//! The miner replaces that merkle root itself before hashing, so a re-serialized
//! template is the SAME job.
//!
//! When the miner treated it as a new job it reinstalled on every poll, bumped the
//! template generation, and every worker then threw away the batch it had just
//! finished - the winning nonce included. This test mines against a sim that churns
//! the template the way the real node does and asserts that mining output keeps
//! flowing ACROSS repeated polls, not just before the first one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use app::poworker::{PoWorkConf, poworker_with_stop};
use testkit::sim::miner_api::{MinerApiSim, MinerPendingStuff};

/// The mining runtime keeps its installed template in process-global state, so the
/// sim-driven tests in this binary must not overlap.
fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn fetch_block_intro(rpcaddr: &str) -> String {
    let url = format!("http://{rpcaddr}/query/miner/pending?stuff=true");
    let body = reqwest::blocking::get(&url)
        .expect("query the simulated miner api")
        .text()
        .expect("read the pending body");
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("parse the pending body as json");
    parsed["block_intro"]
        .as_str()
        .expect("pending answer carries a block_intro")
        .to_string()
}

#[test]
fn a_miner_polling_a_template_mutating_node_still_submits_its_winners() {
    let _guard = test_guard();

    let sim = MinerApiSim::start(MinerPendingStuff::easy_for_test(1));

    // Guard the guard: if the sim ever stops churning the template, this whole
    // test silently stops proving anything, which is exactly how the defect
    // survived a green suite in the first place.
    let first = fetch_block_intro(sim.rpcaddr());
    let second = fetch_block_intro(sim.rpcaddr());
    assert_ne!(
        first, second,
        "the sim must re-serialize block_intro per request like the real fullnode"
    );

    let stop = Arc::new(AtomicBool::new(false));
    // Batches deliberately longer than the miner's pending poll period, so several
    // template refreshes land INSIDE every batch. That is the production shape: a
    // CPU batch self-tunes to ~3 s while the node is polled far more often.
    let cnf = PoWorkConf::test_defaults(sim.rpcaddr().to_string(), 1, 20_000);

    let stop2 = stop.clone();
    let worker = thread::spawn(move || {
        poworker_with_stop(cnf, Some(stop2));
    });

    // Mining has to work at all before the churn question is even meaningful.
    if !sim.wait_for_submit(1, Duration::from_secs(45)) {
        stop.store(true, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(80));
        panic!(
            "the miner submitted nothing at all against a node that refreshes its template on every poll"
        );
    }

    // From here on, measure output ACROSS template refreshes: wait until the miner
    // has re-fetched pending several more times and count the winners it produced
    // in that window. Reinstalling on every poll used to void the in-flight batch
    // of every worker, so this window produced nothing at all.
    const REQUIRED_POLLS: usize = 5;
    const REQUIRED_SUBMITS: usize = 3;
    let polls_before = sim.pending_count();
    let submits_before = sim.submit_count();
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if sim.pending_count() >= polls_before + REQUIRED_POLLS
            && sim.submit_count() >= submits_before + REQUIRED_SUBMITS
        {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let polls = sim.pending_count().saturating_sub(polls_before);
    let submits = sim.submit_count().saturating_sub(submits_before);
    stop.store(true, Ordering::Relaxed);
    thread::sleep(Duration::from_millis(80));

    assert!(
        polls >= REQUIRED_POLLS,
        "expected the miner to re-fetch pending at least {REQUIRED_POLLS} more times, saw {polls}"
    );
    assert!(
        submits >= REQUIRED_SUBMITS,
        "mining output collapsed across template refreshes: only {submits} winners submitted over {polls} pending polls"
    );
    assert_eq!(sim.last_submit().get("height"), Some(&"1".to_string()));

    drop(sim);
    let _ = worker.join();
}
