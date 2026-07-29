//! An existing pool, upgraded in place, carries real miner balances through ONE
//! file. Two of the fixes in this release changed its shape at the same time -
//! the share window gained arrival times and the settlement ledger gained an
//! owed list - and each was written believing it was the only one reading it.
//!
//! This is that file, exactly as the shipped build wrote it, read by the build
//! that replaces it. Nothing here is a fixture invented for the new code: every
//! field is the one the old `state_shot` and the old `PayoutRecord::to_json`
//! emitted, and the assertions are about the money in it.

use hbit_pool::pool_core::{Pplns, split_payout};
use hbit_pool::{
    DEFAULT_SETTLE_SECS, ImmatureBlock, PaidLedger, PayoutRecord, credit_anchor_ms,
    load_immature_blocks, load_owed, load_paid_ledger, load_payout_records,
    load_pending_payout_txs, load_pplns_credit, parse_banked_credit, parse_share_order,
    pplns_horizon_ms, save_settlement_ledger,
};

fn tmp(tag: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("hbit-pool-upgrade-{}-{tag}.json", std::process::id()));
    p.to_string_lossy().to_string()
}

/// The pool state file EXACTLY as the shipped build wrote it: `order` is a bare
/// list of worker ids, `payouts_inflight` rows carry no signed bytes, `immature`
/// entries carry no fee flag, and there is no `owed` list at all.
fn an_old_state_file() -> serde_json::Value {
    serde_json::json!({
        "window": 4096,
        "order": ["w-a", "w-b", "w-a", "w-c", "w-a", "w-b"],
        "accepted": 6,
        "blocks": 2,
        "orphaned": 1,
        "settle_pending_txs": ["aa11"],
        "payouts_inflight": [{
            "hash": "aa11",
            "at": 1_700_000_000u64,
            "node_holds": true,
            "rows": [["w-a", 12], ["w-b", 3]],
        }],
        "paid": {
            "since": 1_600_000_000u64,
            "rows": [{
                "worker": "w-c",
                "units": 41,
                "last_units": 9,
                "last_hash": "bb22",
                "last_at": 1_650_000_000u64,
            }],
        },
        "submitted": [{"height": 901, "hash": "cd".repeat(32)}],
        "immature": [{"height": 900, "hash": "ab".repeat(32), "units": 30}],
    })
}

fn write(path: &str, j: &serde_json::Value) {
    std::fs::write(path, j.to_string()).expect("write the old state file");
}

#[test]
fn an_old_state_file_upgrades_without_losing_a_share_a_debt_or_a_payout() {
    let path = tmp("whole");
    let _ = std::fs::remove_file(&path);
    write(&path, &an_old_state_file());

    // ---- the share window -------------------------------------------------
    // Six shares by three miners, 3:2:1. The old file records no arrival times,
    // so the only reading that does not move money between miners is that they
    // all landed together: credit must come back in the SAME 3:2:1 ratio the
    // old build's headcount split would have used.
    let credit = load_pplns_credit(&path);
    assert_eq!(credit.len(), 3, "a miner was lost from the window: {credit:?}");
    let by: std::collections::HashMap<&str, u64> =
        credit.iter().map(|(w, c)| (w.as_str(), *c)).collect();
    assert!(by["w-a"] > 0, "an upgraded window credits nobody: {credit:?}");
    assert_eq!(by["w-a"], 3 * by["w-c"], "3 shares must weigh 3x1: {credit:?}");
    assert_eq!(by["w-b"], 2 * by["w-c"], "2 shares must weigh 2x1: {credit:?}");
    // And that is exactly the split the old build made by headcount.
    let old_counts = [("w-a", 3u64), ("w-b", 2), ("w-c", 1)];
    for (w, n) in old_counts {
        assert_eq!(
            by[w] * 6,
            n * (by["w-a"] + by["w-b"] + by["w-c"]),
            "the upgrade moved money between miners at {w}"
        );
    }

    // ---- the settlement ledger --------------------------------------------
    // The payout in flight survives with its rows. Its signed bytes are absent,
    // and that absence must NOT read as permission to re-sign the window.
    let recs = load_payout_records(&path);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].hash, "aa11");
    assert!(recs[0].node_holds);
    assert!(recs[0].body_hex.is_empty(), "old records carry no bytes");
    assert_eq!(
        recs[0].rows,
        vec![("w-a".to_string(), 12u64), ("w-b".to_string(), 3u64)]
    );
    assert_eq!(load_pending_payout_txs(&path), vec!["aa11".to_string()]);

    // No owed list in an old file is an empty debt, not a corrupt file.
    assert!(load_owed(&path).is_empty());

    // The paid ledger - every miner's "total paid" - is untouched.
    let paid = load_paid_ledger(&path);
    assert_eq!(paid.since, 1_600_000_000);
    assert_eq!(paid.get("w-c").expect("w-c").units, 41);
    assert_eq!(paid.get("w-c").expect("w-c").last_hash, "bb22");

    // The maturity hold-back survives, and reads as fees-NOT-counted: that file
    // really does hold the subsidy alone, and reading it the other way would
    // distribute the blocks' transaction fees on the first settlement after the
    // upgrade.
    assert_eq!(
        load_immature_blocks(&path),
        vec![ImmatureBlock {
            height: 900,
            hash: "ab".repeat(32),
            units: 30,
            fees_counted: false,
        }]
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn writing_the_new_ledger_into_an_old_file_keeps_the_old_share_window_readable() {
    // The upgrade path in the flesh: the manual payout tool opens the old file,
    // records a fresh payout and a debt, and rewrites the settlement ledger with
    // `save_settlement_ledger` - which preserves every field it does not own,
    // INCLUDING the untimed `order`. The next reader has to cope with a file
    // that is new in one half and old in the other. Nobody tested that shape.
    let path = tmp("mixed");
    let _ = std::fs::remove_file(&path);
    write(&path, &an_old_state_file());

    let mut records = load_payout_records(&path);
    records.push(PayoutRecord {
        hash: "cc33".to_string(),
        at: 1_700_000_100,
        node_holds: false,
        body_hex: "deadbeef".to_string(),
        rows: vec![("w-c".to_string(), 20)],
    });
    let owed = vec![("w-b".to_string(), 7u64)];
    let paid: PaidLedger = load_paid_ledger(&path);
    save_settlement_ledger(
        &path,
        &["aa11".to_string(), "cc33".to_string()],
        &records,
        &owed,
        &paid,
    )
    .expect("rewrite the settlement ledger in place");

    // Nothing the write did not own moved.
    let j: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
    assert_eq!(j["accepted"].as_u64(), Some(6));
    assert_eq!(j["blocks"].as_u64(), Some(2));
    assert_eq!(j["orphaned"].as_u64(), Some(1));
    assert_eq!(j["order"].as_array().map(|a| a.len()), Some(6));
    assert!(
        j["order"][0].is_string(),
        "the untimed window must survive a ledger write untouched"
    );
    assert_eq!(j["submitted"].as_array().map(|a| a.len()), Some(1));
    assert_eq!(j["immature"][0]["units"].as_u64(), Some(30));
    assert!(
        j["immature"][0].get("fees_counted").is_none(),
        "a ledger write must not invent a fee flag it did not check"
    );

    // And the half-old, half-new file reads back whole.
    let credit = load_pplns_credit(&path);
    assert_eq!(credit.len(), 3);
    assert!(credit.iter().all(|(_, c)| *c > 0));
    assert_eq!(load_owed(&path), owed);
    let back = load_payout_records(&path);
    assert_eq!(back.len(), 2);
    assert_eq!(back[0].body_hex, "", "the old record still carries no bytes");
    assert_eq!(back[1].body_hex, "deadbeef");
    assert_eq!(load_paid_ledger(&path).get("w-c").expect("w-c").units, 41);
    assert_eq!(
        load_immature_blocks(&path),
        vec![ImmatureBlock {
            height: 900,
            hash: "ab".repeat(32),
            units: 30,
            fees_counted: false,
        }]
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_untimed_window_is_read_as_one_horizon_old_whatever_the_file_says() {
    // The fallback stamp decides how much credit an upgraded window carries, and
    // getting it wrong in either direction moves money: stamp it NOW and every
    // share reads as brand new, so the whole window earns nothing and the first
    // miner to submit after the restart takes the settlement; stamp it at zero
    // and the shares are capped at the horizon anyway, which is the same as one
    // horizon back. One horizon back is the only stamp that reproduces the old
    // build's split.
    let old = an_old_state_file();
    assert!(parse_banked_credit(&old).is_empty());
    let rows = parse_share_order(&old, 12_345);
    assert_eq!(rows.len(), 6);
    assert!(
        rows.iter().all(|(_, at)| *at == 12_345),
        "every share in an untimed file must weigh the same: {rows:?}"
    );

    // The horizon a file with no `credit_horizon_ms` is read at is the
    // documented default, so an upgraded pool splits the way its `/terms` said.
    assert_eq!(pplns_horizon_ms(DEFAULT_SETTLE_SECS), 300_000);
}

/// The state file a live pool leaves behind: shares stamped with the pool's own
/// clock, and the credit of everything the newest shares evicted.
fn a_stopped_pool(horizon_ms: u64, stopped_at: u64, window: usize) -> serde_json::Value {
    let mut p = Pplns::new(window, horizon_ms);
    // "honest" mines the whole interval and sends its shares in as it finds them.
    let start = stopped_at - horizon_ms;
    for i in 0..window as u64 {
        p.record("honest", start + i * (horizon_ms / window as u64).max(1));
    }
    // "hoarder" mines the same interval, submits nothing, and dumps a full
    // window's worth in the last second before the pool is stopped - which
    // evicts every one of honest's shares.
    for i in 0..window as u64 {
        p.record("hoarder", stopped_at - 1_000 + i);
    }
    serde_json::json!({
        "window": window,
        "credit_horizon_ms": horizon_ms,
        "order": p.snapshot().iter().map(|(w, at)| serde_json::json!([w, at]))
            .collect::<Vec<_>>(),
        "banked": p.banked_snapshot().into_iter().map(|(at, rows)| serde_json::json!({
            "at": at,
            "rows": rows,
        })).collect::<Vec<_>>(),
    })
}

#[test]
fn the_manual_payout_tool_does_not_hand_a_hoarder_the_window_by_being_run_late() {
    // hbit-pool-payout is the second settler, and the one an operator reaches
    // for with the server DOWN - which it must be, or the tool cannot take the
    // settlement lock. So the file it reads is frozen: no share enters or leaves
    // that window while the server is stopped.
    //
    // Valuing it at the WALL CLOCK let the whole window age together anyway.
    // Past one horizon every share caps out at the same figure, so credit
    // flattens back into a HEADCOUNT - and the banked credit of the miners the
    // dump evicted expires completely. A miner that withheld a window's worth
    // and dumped it just before the pool was stopped then takes the entire
    // payout, which is the exact attack the timestamps were added to stop. Five
    // minutes between stopping the pool and running the payout was enough.
    let horizon = pplns_horizon_ms(DEFAULT_SETTLE_SECS);
    let window = 64;
    let stopped_at = 1_700_000_000_000u64;
    let j = a_stopped_pool(horizon, stopped_at, window);

    let path = tmp("hoarder");
    let _ = std::fs::remove_file(&path);
    write(&path, &j);

    // The headcount at the moment the pool stopped: the hoarder owns all of it.
    let order = parse_share_order(&j, 0);
    assert_eq!(order.len(), window);
    assert!(order.iter().all(|(w, _)| w == "hoarder"));

    // Read the file the way the tool reads it, and split the way it splits.
    let credit = load_pplns_credit(&path);
    let paid: std::collections::HashMap<String, u64> =
        split_payout(1_000, 0, 0, &credit).into_iter().collect();
    let honest = *paid.get("honest").unwrap_or(&0);
    let hoarder = *paid.get("hoarder").unwrap_or(&0);
    assert!(
        honest > 900 && hoarder < 100,
        "a withheld batch took the manual payout: honest={honest} hoarder={hoarder} \
         credit={credit:?}"
    );

    // And the answer does not depend on WHEN the operator gets round to running
    // it: the file is valued at the last moment the pool was accounting, so an
    // hour later settles identically to a second later.
    assert_eq!(
        credit_anchor_ms(&j, stopped_at + 1_000),
        credit_anchor_ms(&j, stopped_at + horizon * 100),
        "the split must not depend on how long the operator took"
    );
    assert_eq!(credit, load_pplns_credit(&path), "the same file, twice");

    // A file with no times to anchor to still values at the wall clock, which is
    // what keeps an upgraded (untimed) window weighing anything at all.
    let untimed = serde_json::json!({"order": ["a", "b"]});
    assert_eq!(credit_anchor_ms(&untimed, 4_242), 4_242);

    let _ = std::fs::remove_file(&path);
}
