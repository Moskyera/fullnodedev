//! Pick (and prove) the `[mint]` settings for a local chain the HBIT pool will
//! actually serve.
//!
//! WHY THIS EXISTS
//!
//! The pool refuses to hand out work when a share is not worth counting:
//! `share_cost_bits >= 16` AND `achieved_share_factor >= 18`, and those two add
//! up to the leading zero bits of the NETWORK target. So the pool needs a chain
//! sitting at >= 34 leading zero bits. A freshly started non-mainnet chain does
//! not: off mainnet ASERT anchors at `difficulty_adjust_blocks + 2` with the
//! fixed constant 0xe9cfffff, which is exactly 22 leading zero bits.
//!
//! The only knob that moves where the chain SETTLES is `each_block_target_time`.
//! ASERT is an equilibrium controller: it drives the target until a block takes
//! `each_block_target_time` seconds, so the resting difficulty of a private
//! chain is whatever your own hashrate can do in that time. Pick the target time
//! and you pick the difficulty. That is the whole trick, and it needs no code
//! change at all, so mainnet consensus is untouched by construction.
//!
//! This example does not model that with a formula. It runs the REAL difficulty
//! function (`hbit_pool::difficulty::next_difficulty`, which `hbit-asert-check`
//! has already proven byte-identical to the node against real mainnet history)
//! block by block, and reports the wall clock. Compare its answer against a real
//! run; if they disagree, believe the run.
//!
//! USAGE
//!   cargo run -p hbit-pool --example testnet_rig_plan -- \
//!       <hashrate_MHs> [adjust_blocks] [target_time_secs] [max_sim_secs] [min_block_secs]
//!
//! With no target time it SEARCHES for one and prints a table, which is how you
//! choose the number to put in the ini.
//!
//! Note on hashrate: x16rs repeats `height/50000 + 1` times (capped at 16), so a
//! private chain below height 50000 runs at repeat=1. Use your repeat=1 figure
//! here, not the mainnet repeat=16 one. They differ by more than 10x.

use hbit_pool::difficulty::{ChainParams, next_difficulty};
use hbit_pool::pool_core::share_cost_bits;

/// What the pool demands of the NETWORK target before it will serve work.
/// Mirrors MIN_SHARE_FACTOR (18) + MIN_SHARE_COST_BITS (16) in server.rs.
const POOL_MIN_NETWORK_BITS: u32 = 34;
/// Above this a block stops being winnable in a sitting; not a hard rule, just
/// the top of the band this rig aims for.
const BAND_MAX_BITS: u32 = 38;

struct Outcome {
    /// Seconds of wall clock until the chain first reaches POOL_MIN_NETWORK_BITS.
    reach_secs: Option<u64>,
    /// Height at that moment.
    reach_height: Option<u64>,
    /// Seconds spent inside [POOL_MIN_NETWORK_BITS, BAND_MAX_BITS] within the sim.
    band_secs: u64,
    /// Bits at the end of the simulated window.
    final_bits: u32,
    /// Expected seconds for one block at the final difficulty.
    final_block_secs: f64,
    /// True if the sim ran out of time before reaching the band.
    timed_out: bool,
}

/// Walk the chain forward through the real difficulty rule.
///
/// The model has exactly one assumption: a block at B leading zero bits takes
/// 2^B / hashrate seconds, floored at 1 because `chain/src/verify.rs` rejects
/// `blk_time <= prev_blk_time` in a release build. Everything else is the
/// shipped consensus code.
fn simulate(
    hashrate: f64,
    adjust_blocks: u64,
    target_time: u64,
    max_secs: u64,
    min_block_secs: u64,
) -> Outcome {
    let p = ChainParams::testnet(adjust_blocks, target_time);
    // Bootstrap heights are LOWEST_DIFFICULTY (every hash wins) and the anchor
    // block itself is the fixed start target; both are one second each because
    // of the timestamp floor.
    let anchor_time: u64 = p.asert_height; // one second per bootstrap block from t=0
    let mut clock = anchor_time;
    let mut height = p.asert_height;
    let mut prev_diff = next_difficulty(&p, p.asert_height, anchor_time, 0, 0).0;

    let mut reach_secs = None;
    let mut reach_height = None;
    let mut band_secs = 0u64;
    let mut bits = share_cost_bits(&next_difficulty(&p, p.asert_height, anchor_time, 0, 0).1);
    let mut block_secs = 2f64.powi(bits as i32) / hashrate;

    while clock - anchor_time < max_secs {
        // How long this block takes at the difficulty now in force.
        block_secs = 2f64.powi(bits as i32) / hashrate;
        let step = (block_secs.round() as u64).max(min_block_secs);
        if (POOL_MIN_NETWORK_BITS..=BAND_MAX_BITS).contains(&bits) {
            band_secs += step;
        }
        clock += step;
        height += 1;
        let (num, hash) = next_difficulty(&p, height, clock, prev_diff, anchor_time);
        prev_diff = num;
        bits = share_cost_bits(&hash);
        if bits >= POOL_MIN_NETWORK_BITS && reach_secs.is_none() {
            reach_secs = Some(clock - anchor_time);
            reach_height = Some(height);
        }
    }
    Outcome {
        reach_secs,
        reach_height,
        band_secs,
        final_bits: bits,
        final_block_secs: block_secs,
        timed_out: reach_secs.is_none(),
    }
}

fn hms(s: u64) -> String {
    format!("{:02}h{:02}m{:02}s", s / 3600, (s % 3600) / 60, s % 60)
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mhs: f64 = a
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            eprintln!(
                "usage: testnet_rig_plan <hashrate_MHs> [adjust_blocks] [target_time_secs] [max_sim_secs] [min_block_secs]"
            );
            std::process::exit(2)
        });
    let hashrate = mhs * 1e6;
    let adjust_blocks: u64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);
    let explicit_tt: Option<u64> = a.get(3).and_then(|s| s.parse().ok());
    let max_secs: u64 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(6 * 3600);
    // Floor on how long a block takes in practice. The consensus floor is 1
    // second (block_build.rs nextts = max(now, prev_ts+1)), but a real worker
    // also spends time polling for a template and posting the solution, so the
    // cheap early blocks land slower than the pure hash cost suggests. A run
    // measured on this rig arrived at 34 bits about 20% later than the 1-second
    // model; pass 2 here to see that bracket.
    let min_block_secs: u64 = a.get(5).and_then(|s| s.parse().ok()).unwrap_or(1).max(1);

    let p = ChainParams::testnet(adjust_blocks, explicit_tt.unwrap_or(300));
    let anchor_bits = share_cost_bits(&next_difficulty(&p, p.asert_height, 0, 0, 0).1);

    println!("== HBIT local-chain rig plan ==");
    println!("hashrate            = {mhs} MH/s (x16rs repeat=1)");
    println!("difficulty_adjust_blocks = {adjust_blocks}  -> ASERT anchors at height {}", p.asert_height);
    println!("anchor difficulty   = {anchor_bits} leading zero bits (fixed constant 0xe9cfffff)");
    println!("pool needs          >= {POOL_MIN_NETWORK_BITS} network bits (share_bits 18 + share cost 16)");
    println!("simulation window   = {}\n", hms(max_secs));

    // Equilibrium: a block at B bits takes 2^B/hashrate seconds, so the chain
    // rests where that equals each_block_target_time.
    let eq_bits = |tt: u64| (hashrate * tt as f64).log2();
    let tt_for = |bits: f64| (2f64.powf(bits) / hashrate).round() as u64;

    match explicit_tt {
        Some(tt) => {
            let o = simulate(hashrate, adjust_blocks, tt, max_secs, min_block_secs);
            report(tt, eq_bits(tt), &o);
            // The verdict is the exit code, not the text. A script must be able
            // to reject an unviable target time without reading English.
            if o.reach_secs.is_none() {
                std::process::exit(1);
            }
        }
        None => {
            println!(
                "{:>9}  {:>8}  {:>12}  {:>8}  {:>12}  {:>10}",
                "target_t", "eq_bits", "reach>=34", "at_hei", "in_band", "blk_at_end"
            );
            println!("{}", "-".repeat(70));
            // Candidate target times that put equilibrium at 34..39 bits.
            for b in POOL_MIN_NETWORK_BITS..=(BAND_MAX_BITS + 1) {
                let tt = tt_for(b as f64).max(1);
                let o = simulate(hashrate, adjust_blocks, tt, max_secs, min_block_secs);
                println!(
                    "{:>9}  {:>8.2}  {:>12}  {:>8}  {:>12}  {:>9.0}s",
                    tt,
                    eq_bits(tt),
                    o.reach_secs.map(hms).unwrap_or_else(|| "NEVER".into()),
                    o.reach_height
                        .map(|h| h.to_string())
                        .unwrap_or_else(|| "-".into()),
                    hms(o.band_secs),
                    o.final_block_secs
                );
            }
            println!(
                "\nPick the row with the smallest `reach>=34` that still leaves a long `in_band`,\n\
                 put that target_t in [mint].each_block_target_time, then re-run this with it\n\
                 as argument 3 for the full report."
            );
        }
    }
}

fn report(tt: u64, eq: f64, o: &Outcome) {
    println!("each_block_target_time = {tt}s   (equilibrium ~{eq:.2} bits)");
    match (o.reach_secs, o.reach_height) {
        (Some(s), Some(h)) => {
            println!("reaches {POOL_MIN_NETWORK_BITS} bits after {} of mining, at height {h}", hms(s));
            println!(
                "stays in the {POOL_MIN_NETWORK_BITS}..{BAND_MAX_BITS} band for {} of the simulated window",
                hms(o.band_secs)
            );
            println!(
                "at the end of the window: {} bits, ~{:.0}s per block",
                o.final_bits, o.final_block_secs
            );
            println!("\nPLAN IS VIABLE.");
        }
        _ => {
            println!(
                "NEVER reaches {POOL_MIN_NETWORK_BITS} bits in the window (ends at {} bits, ~{:.0}s per block).",
                o.final_bits, o.final_block_secs
            );
            println!("timed_out={}", o.timed_out);
            println!("\nPLAN IS NOT VIABLE at this target time.");
        }
    }
}
