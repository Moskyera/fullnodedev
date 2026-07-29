//! Watch a local (non-mainnet) chain climb to a difficulty the HBIT pool will serve.
//!
//! This is the MEASURING instrument for the rig in
//! `scripts/hbit-local-chain-rig/`. It does not model anything: it polls the
//! node and reports the difficulty the chain actually stored, converted to
//! leading zero bits by the SAME function the pool's own admission check uses
//! (`pool_core::share_cost_bits` over `pool_core::network_target_hash`), so the
//! number printed here is the number `check_share_target` will see.
//!
//! Exit code is the result, because that is the only part a script may trust:
//!   0  the chain reached `--want` bits inside the deadline
//!   1  the deadline passed first (prints the best it managed)
//!   2  bad arguments / the node could not be read
//!
//! Usage:
//!   cargo run --release -p hbit-pool --example local_chain_watch -- \
//!       <node_base> <want_bits> <deadline_secs> [poll_secs]

use hbit_pool::pool_core::{
    achieved_share_factor, network_target_hash, share_cost_bits, share_target_hash,
};
use hbit_pool::{find_u64, get_json, http_client};
use std::time::{Duration, Instant};

/// server.rs MIN_SHARE_FACTOR: how much easier a share may be than a block.
const MIN_SHARE_FACTOR: u32 = 18;
/// server.rs MIN_SHARE_COST_BITS: what the share itself must cost.
const MIN_SHARE_COST_BITS: u32 = 16;

/// Reproduce the pool's own admission gate for a given network difficulty.
///
/// server.rs derives exactly these two numbers from exactly these two calls and
/// hands them to `check_share_target`, so if this says yes the pool says yes.
/// `check_share_target` is private to that binary, which is why the inputs are
/// recomputed here rather than the decision being imported.
fn pool_would_serve(difficulty: u32, share_bits: u32) -> (bool, u32, u32) {
    let network = network_target_hash(difficulty);
    let share = share_target_hash(difficulty, share_bits);
    let achieved = achieved_share_factor(&network, &share);
    let cost = share_cost_bits(&share);
    (
        achieved >= MIN_SHARE_FACTOR && cost >= MIN_SHARE_COST_BITS,
        achieved,
        cost,
    )
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let node = a
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    let node = node.trim_end_matches('/').to_string();
    let want: u32 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(34);
    let deadline: u64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(3600);
    let poll: u64 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(5);

    let client = http_client();
    let started = Instant::now();

    // Read the tip's height, timestamp and stored difficulty.
    let tip = |c: &reqwest::blocking::Client| -> Option<(u64, u64, u32)> {
        let h = find_u64(&get_json(c, &format!("{node}/query/latest")), "height")?;
        let b = get_json(c, &format!("{node}/query/block/intro?height={h}"));
        Some((
            h,
            find_u64(&b, "timestamp")?,
            find_u64(&b, "difficulty")? as u32,
        ))
    };

    let Some((h0, _, d0)) = tip(&client) else {
        eprintln!("could not read the chain tip from {node}");
        std::process::exit(2);
    };
    println!("== local chain watch ==");
    println!("node      = {node}");
    println!("want      >= {want} network leading-zero bits");
    println!("deadline  = {deadline}s, poll every {poll}s");
    println!(
        "start     = height {h0}, difficulty {d0}, {} bits\n",
        share_cost_bits(&network_target_hash(d0))
    );
    println!(
        "{:>8}  {:>8}  {:>6}  {:>12}  {:>9}  {:>10}",
        "elapsed", "height", "bits", "difficulty", "blk_secs", "blocks"
    );

    let mut last_height = h0;
    let mut last_ts: Option<u64> = None;
    let mut best_bits = share_cost_bits(&network_target_hash(d0));
    // First time each milestone was seen, as (bits, elapsed_secs, height).
    let mut milestones: Vec<(u32, u64, u64)> = Vec::new();

    loop {
        let elapsed = started.elapsed().as_secs();
        if let Some((h, ts, d)) = tip(&client) {
            let bits = share_cost_bits(&network_target_hash(d));
            let blk_secs = match last_ts {
                Some(prev) if h > last_height => {
                    format!("{}", ts.saturating_sub(prev) / (h - last_height).max(1))
                }
                _ => "-".to_string(),
            };
            if h != last_height || bits != best_bits {
                println!(
                    "{:>7}s  {:>8}  {:>6}  {:>12}  {:>9}  {:>10}",
                    elapsed,
                    h,
                    bits,
                    d,
                    blk_secs,
                    h.saturating_sub(h0)
                );
            }
            if bits > best_bits {
                best_bits = bits;
                milestones.push((bits, elapsed, h));
            }
            last_height = h;
            last_ts = Some(ts);
            if bits >= want {
                println!("\nREACHED {want} bits at height {h} after {elapsed}s.");
                print_milestones(&milestones, want);
                println!(
                    "\nA block at {bits} bits costs 2^{bits} hashes on average.\n\
                     The pool's own admission gate at this difficulty ({d}):"
                );
                for sb in [MIN_SHARE_FACTOR, 20, 24] {
                    let (ok, achieved, cost) = pool_would_serve(d, sb);
                    let verdict = if ok { "SERVES" } else { "REFUSES" };
                    println!(
                        "  share_bits {sb:>2}: achieved {achieved:>2} (min {MIN_SHARE_FACTOR}), \
                         cost bits {cost:>2} (min {MIN_SHARE_COST_BITS})  ->  {verdict}"
                    );
                }
                std::process::exit(0);
            }
        } else {
            println!("{:>7}s  (node not answering yet)", elapsed);
        }
        if elapsed >= deadline {
            println!("\nDEADLINE: only reached {best_bits} bits (wanted {want}) in {elapsed}s.");
            print_milestones(&milestones, want);
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_secs(poll.max(1)));
    }
}

fn print_milestones(ms: &[(u32, u64, u64)], want: u32) {
    if ms.is_empty() {
        println!("(difficulty never moved)");
        return;
    }
    println!("\nfirst sighting of each bit level:");
    for (bits, secs, hei) in ms {
        let mark = if *bits >= want { "  <- pool servable" } else { "" };
        println!("  {bits:>2} bits  at {secs:>6}s  height {hei}{mark}");
    }
}
