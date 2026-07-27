//! Shared helpers for the pool spikes: HTTP glue + off-node block assembly that
//! mirrors the node's `impl_packing_next_block` for a block containing a
//! coinbase plus optional extra transactions. Targets a fresh local testnet
//! (bootstrap LOWEST_DIFFICULTY); does not reproduce mainnet ASERT difficulty.

pub mod difficulty;
pub mod pool_core;

use difficulty::ChainParams;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use basis::difficulty::*;
use basis::interface::*;
use field::*;
use protocol::block::*;
use protocol::transaction::*;
use sys::*;

use serde_json::Value;
use zeroize::Zeroizing;

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

/// The largest balance the pool will act on, in units of 0.1 HAC. Hacash's whole
/// coin supply is tens of millions of HAC, so anything past 100 billion HAC is a
/// corrupt or hostile answer, not a wallet. Refusing it keeps a bad number out of
/// the payout split instead of turning it into a maximal payout plan.
pub const MAX_PLAUSIBLE_UNITS: u64 = 1_000_000_000_000;

/// A node "mantissa:unit" balance expressed in whole units of 0.1 HAC (unit 247).
///
/// Hacash stores amounts normalized (trailing zeros stripped, unit raised), so a
/// balance like 4.9 HAC comes back as "49:246", not "490:247". FLOOR to 0.1-HAC
/// granularity, keeping the whole part, rather than discarding a balance just
/// because it is finer than 0.1 HAC. Shared by the pool server and the payout
/// tool so both value a balance identically.
///
/// `None` means the node's answer was missing a separator, unparseable, or
/// larger than any real wallet: the caller must SKIP settlement rather than pay
/// out on it. Saturating to u64::MAX here (as this used to) means "infinite
/// money" to `distributable_units` and `split_payout`, which then plan a payout
/// of the whole u64 range off one malformed response. An EMPTY string is not an
/// error: the node simply omits the field for an address holding nothing.
pub fn balance_units(bal: &str) -> Option<u64> {
    if bal.trim().is_empty() {
        return Some(0);
    }
    let (m, u) = bal.split_once(':')?;
    let (Ok(m), Ok(u)) = (m.trim().parse::<u64>(), u.trim().parse::<i64>()) else {
        return None;
    };
    let units = if u >= 247 {
        let exp = u - 247;
        if exp > 18 {
            return None; // beyond any representable wallet, not a big balance
        }
        m.checked_mul(10u64.pow(exp as u32))?
    } else {
        let exp = 247 - u;
        if exp > 18 {
            return Some(0); // finer than 0.1 HAC: floors to nothing payable
        }
        m / 10u64.pow(exp as u32)
    };
    (units <= MAX_PLAUSIBLE_UNITS).then_some(units)
}

/// The coinbase subsidy of the block at `height`, in units of 0.1 HAC. The pool
/// mines coinbase-only blocks, so this is the entire income a found block brings
/// into the wallet (`block_reward` is a whole number of HAC = unit 248).
pub fn block_reward_units(height: u64) -> u64 {
    mint::genesis::block_reward_number(height) as u64 * 10
}

/// How deep a payout transaction must be buried before the pool stops tracking
/// it. The node keeps up to `unstable_block` (4) blocks reorg-able, so a payout
/// that is only 1-3 confirmations deep can still come back to the mempool;
/// forgetting it that early lets the next cycle pay the same PPLNS window a
/// second time. 6 keeps a margin over the node's own window.
pub const PAYOUT_MATURITY_DEPTH: u64 = 6;

/// What the node says about a payout transaction we previously submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayoutTxState {
    /// Still waiting in the mempool.
    Pending,
    /// Mined, but shallower than [`PAYOUT_MATURITY_DEPTH`] - a reorg could still
    /// put it back in the mempool, so it is not finished with.
    Confirming(u64),
    /// Mined and buried deep enough that a reorg cannot undo it.
    Buried(u64),
    /// The node definitively does not know this hash: it was rejected, never
    /// relayed, or dropped from the mempool. Settling again is the right move.
    Gone,
    /// We could not reach the node, or could not understand its answer. This is
    /// NOT a resolution: treating it as one is exactly what opens a double-payout
    /// window, so the caller must keep the hash and skip the cycle.
    Unknown,
}

/// Classify a `/query/transaction?hash=...` response. Fails SAFE: anything that
/// is not an unambiguous verdict from the node comes back as `Unknown`, and a
/// shallow confirmation counts as still in flight.
pub fn classify_payout_tx(j: &Value) -> PayoutTxState {
    // get_json encodes a transport failure as {"http_error": "..."} and a
    // non-JSON body as a bare string. Neither is the node speaking.
    if !j.is_object() || j.get("http_error").is_some() {
        return PayoutTxState::Unknown;
    }
    let Some(ret) = find_u64(j, "ret") else {
        return PayoutTxState::Unknown;
    };
    if ret != 0 {
        return PayoutTxState::Gone; // the node answered "transaction not found"
    }
    let is_pending = j
        .get("data")
        .and_then(|d| d.get("pending"))
        .and_then(|v| v.as_bool())
        .or_else(|| j.get("pending").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    if is_pending {
        return PayoutTxState::Pending;
    }
    // ret=0 and not pending means mined; the node reports the burial depth.
    match find_u64(j, "confirm") {
        Some(d) if d >= PAYOUT_MATURITY_DEPTH => PayoutTxState::Buried(d),
        Some(d) => PayoutTxState::Confirming(d),
        // ret=0 with neither `pending` nor `confirm` is a shape we do not
        // recognise; unresolved is the safe reading.
        None => PayoutTxState::Unknown,
    }
}

/// How many times to ask the node whether it really holds a payout we just
/// submitted, and how long to wait between asks. `/submit/transaction` answers
/// ret=0 the moment the API has validated the transaction and handed it to a
/// background task, so the node needs a moment before its own view of the hash
/// means anything.
pub const ADMIT_POLL_TRIES: u32 = 10;
pub const ADMIT_POLL_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// What the NODE says it holds after we submitted a payout transaction.
///
/// `/submit/transaction` returning ret=0 is NOT this answer. The node validates
/// the transaction synchronously and then performs the mempool insert on a
/// background task whose result it DISCARDS, so the API reports "ok" for a
/// transaction the mempool went on to refuse - and the pool then reports a
/// payout that does not exist. The node's own view of the hash is the only
/// evidence that a payout is really in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The node reports the transaction: waiting in its mempool, or already mined.
    Held,
    /// The node answered definitively that it does not know this transaction.
    /// It was never inserted, so it was never relayed either.
    Missing,
    /// No usable answer (node unreachable, unparseable reply). NOT a resolution:
    /// the payout may well be in flight, so it must stay tracked.
    Unresolved,
}

/// Map the node's `/query/transaction` answer to an admission verdict.
pub fn admission_of(j: &Value) -> Admission {
    match classify_payout_tx(j) {
        PayoutTxState::Pending | PayoutTxState::Confirming(_) | PayoutTxState::Buried(_) => {
            Admission::Held
        }
        PayoutTxState::Gone => Admission::Missing,
        PayoutTxState::Unknown => Admission::Unresolved,
    }
}

/// Ask the node whether it really holds `txhash`, retrying while it has not made
/// up its mind. The insert runs on a background task, so an immediate "not
/// found" only becomes a verdict once the node has had time to do it.
pub fn verify_admitted(
    client: &reqwest::blocking::Client,
    node: &str,
    txhash: &str,
) -> Admission {
    let mut last = Admission::Unresolved;
    for attempt in 0..ADMIT_POLL_TRIES {
        let j = get_json(client, &format!("{node}/query/transaction?hash={txhash}"));
        match admission_of(&j) {
            Admission::Held => return Admission::Held,
            other => last = other,
        }
        if attempt + 1 < ADMIT_POLL_TRIES {
            std::thread::sleep(ADMIT_POLL_DELAY);
        }
    }
    last
}

/// What a settlement may actually pay out: the wallet balance MINUS income a
/// reorg could still take back, MINUS the fee reserve. `None` means "nothing
/// spendable, do not settle this cycle".
///
/// `immature_units` is the coinbase of blocks the pool found that are not yet
/// buried deep enough to be final. Distributing that and then losing the block
/// to a reorg is an unrecoverable operator loss: the income disappears from the
/// canonical chain while the payout transaction that spent it stays valid.
///
/// All arithmetic saturates, so an out-of-range reserve can never wrap the
/// guard open the way `reserve + 1` used to.
pub fn distributable_units(
    balance_units: u64,
    immature_units: u64,
    reserve_units: u64,
) -> Option<u64> {
    let matured = balance_units.saturating_sub(immature_units);
    if matured <= reserve_units.saturating_add(1) {
        return None;
    }
    Some(matured - reserve_units)
}

/// Atomic file write (temp + optional fsync + rename) so a crash or a full disk
/// mid-write can never leave a truncated or corrupt file behind. `durable`
/// fsyncs before the rename.
pub fn atomic_write(path: &str, body: &[u8], durable: bool) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = format!("{path}.tmp.{}", std::process::id());
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body)?;
        if durable {
            let _ = f.sync_all();
        }
    }
    std::fs::rename(&tmp, path)
}

/// The pool's accounting file for `wallet_file`. The auto-settle server and the
/// manual payout tool MUST agree on this path: it carries the ONE pending-payout
/// ledger that stops the two of them paying the same PPLNS window twice.
pub fn pool_state_path(wallet_file: &str) -> String {
    format!("{wallet_file}.state.json")
}

fn read_state_json(state_file: &str) -> Option<Value> {
    let txt = std::fs::read_to_string(state_file).ok()?;
    let j: Value = serde_json::from_str(&txt).ok()?;
    j.is_object().then_some(j)
}

/// The shared pending-payout ledger. A missing or corrupt file reads as an empty
/// ledger (the server rewrites that file wholesale and reports the corruption).
pub fn load_pending_payout_txs(state_file: &str) -> Vec<String> {
    let Some(j) = read_state_json(state_file) else {
        return Vec::new();
    };
    j.get("settle_pending_txs")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Rolling PPLNS window: the last N accepted shares decide the payout split.
pub const PPLNS_WINDOW: usize = 4096;

/// Rebuild the PPLNS share counts from the pool's own accounting file.
///
/// The manual payout tool needs this because the server holds the wallet's
/// settlement lock for its whole run: if the tool is able to settle at all then
/// the server is stopped, so its `/stats` endpoint cannot answer and the file it
/// left behind is the authority on who is owed what.
pub fn load_pplns_counts(state_file: &str) -> Vec<(String, u64)> {
    let Some(j) = read_state_json(state_file) else {
        return Vec::new();
    };
    let window = j
        .get("window")
        .and_then(|v| v.as_u64())
        .unwrap_or(PPLNS_WINDOW as u64) as usize;
    let order: Vec<String> = j
        .get("order")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if order.is_empty() {
        return Vec::new();
    }
    pool_core::Pplns::restore(window, order).counts()
}

/// Total held-back (not yet final) block income recorded by the pool server, in
/// units of 0.1 HAC. The manual payout tool reads it so it applies the SAME
/// maturity gate as the automatic settlement instead of paying at the tip.
pub fn load_immature_units(state_file: &str) -> u64 {
    let Some(j) = read_state_json(state_file) else {
        return 0;
    };
    j.get("immature")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("units").and_then(|v| v.as_u64()))
                .sum()
        })
        .unwrap_or(0)
}

/// Replace `settle_pending_txs` in the pool state file, preserving every other
/// field the server keeps there (share window, counters, immature income).
pub fn save_pending_payout_txs(state_file: &str, hashes: &[String]) -> std::io::Result<()> {
    let mut j = read_state_json(state_file).unwrap_or_else(|| serde_json::json!({}));
    j["settle_pending_txs"] = serde_json::json!(hashes);
    atomic_write(state_file, j.to_string().as_bytes(), true)
}

/* ---------------------------------------------------------------------------
 * The pool's money terms, in ONE place.
 *
 * `/terms` reads these same constants and `settle_once` / `hbit-pool-payout` apply
 * them, so what the pool advertises cannot drift from what it does. Change a
 * number here and every place that states it changes with it.
 * ------------------------------------------------------------------------- */

/// The amount unit the pool accounts in: 0.1 HAC (Hacash amount unit 247). Every
/// payout it plans, submits and reports is a whole number of these.
pub const PAYOUT_UNIT: u8 = 247;

/// `units` of 0.1 HAC as the chain's OWN money type. The pool never renders money
/// as a float or a hand-rolled decimal: what a miner is shown is exactly what the
/// transaction carries.
pub fn payout_amount(units: u64) -> Amount {
    Amount::coin(units, PAYOUT_UNIT)
}

/// The network fee ONE settlement transaction carries: 0.01 HAC. It comes out of
/// the reserve below, never out of a miner's share.
pub fn chunk_tx_fee() -> Amount {
    Amount::coin(1, 246)
}

/// The pool's own fee, in units of 0.1 HAC, taken off the top of a settlement
/// before it is split. It is ZERO: this pool skims nothing.
pub const POOL_FEE_UNITS: u64 = 0;

/// Held back from every settlement so the wallet can always fund the per-chunk
/// network fee above. This is NOT a fee: it stays in the pool wallet and a later
/// cycle distributes whatever of it is no longer needed.
pub const SETTLE_RESERVE_UNITS: u64 = 5;

/// The smallest payout a settlement will include, in units of 0.1 HAC. A worker
/// whose share of a cycle rounds below this is paid nothing THAT CYCLE; the money
/// is never taken from anyone - it stays in the pool wallet and is part of the
/// next cycle's distributable balance.
pub const PAYOUT_DUST_UNITS: u64 = 1;

/// Recipients per settlement transaction. The node enforces TX_ACTIONS_MAX = 200
/// actions, so stay safely under it: a large payout is chunked, never rejected.
pub const PAYOUT_CHUNK: usize = 190;

/* ---------------------------------------------------------------------------
 * Per-worker settlement ledger.
 *
 * `settle_pending_txs` alone can only say that SOME payout is in flight. A miner
 * needs to know what is in flight FOR IT, what it has actually been paid, and
 * when - so the pool keeps the exact per-recipient rows of every payout it
 * submits, and folds them into a paid ledger when, and only when, the node
 * reports that transaction buried.
 * ------------------------------------------------------------------------- */

/// One settlement transaction this pool submitted, with the exact amounts it
/// carries for each recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayoutRecord {
    pub hash: String,
    /// Unix seconds when the pool submitted it.
    pub at: u64,
    /// Did the NODE confirm it holds this transaction? `false` means it was
    /// submitted but the node's verdict could not be read: it may well be in
    /// flight, so it stays tracked, but nothing about it is claimed.
    pub node_holds: bool,
    /// (worker address, units of 0.1 HAC) exactly as the transaction pays them.
    pub rows: Vec<(String, u64)>,
}

impl PayoutRecord {
    /// Total this transaction pays, in units of 0.1 HAC.
    pub fn units(&self) -> u64 {
        self.rows.iter().map(|(_, u)| *u).fold(0u64, |a, b| a.saturating_add(b))
    }

    /// What this transaction pays ONE worker.
    pub fn units_for(&self, worker: &str) -> u64 {
        self.rows
            .iter()
            .filter(|(w, _)| w == worker)
            .map(|(_, u)| *u)
            .fold(0u64, |a, b| a.saturating_add(b))
    }

    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "hash": self.hash,
            "at": self.at,
            "node_holds": self.node_holds,
            "rows": self.rows.iter()
                .map(|(w, u)| serde_json::json!([w, u]))
                .collect::<Vec<_>>(),
        })
    }

    /// Rebuild one record. A row the file cannot describe is DROPPED rather than
    /// guessed at: an unreadable amount must never become a number a miner is
    /// shown.
    pub fn from_json(v: &Value) -> Option<Self> {
        let hash = v.get("hash")?.as_str()?.to_string();
        if hash.is_empty() {
            return None;
        }
        let rows = v
            .get("rows")?
            .as_array()?
            .iter()
            .filter_map(|r| {
                let a = r.as_array()?;
                Some((a.first()?.as_str()?.to_string(), a.get(1)?.as_u64()?))
            })
            .collect();
        Some(Self {
            hash,
            at: v.get("at").and_then(|x| x.as_u64()).unwrap_or(0),
            node_holds: v
                .get("node_holds")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            rows,
        })
    }
}

/// What this pool's ledger has actually paid ONE worker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaidRow {
    /// Total confirmed paid, in units of 0.1 HAC. Only ever grows.
    pub units: u64,
    /// The most recent confirmed payout: amount, transaction, and when the pool
    /// saw the node bury it.
    pub last_units: u64,
    pub last_hash: String,
    pub last_at: u64,
}

/// Confirmed payouts per worker.
///
/// `since` is reported next to every total on purpose: this is what the pool has
/// paid SINCE THIS LEDGER EXISTED, not what a worker has ever earned. A pool
/// whose state file was lost starts a new ledger, and a miner must be able to see
/// that rather than read a total that silently means something else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaidLedger {
    pub since: u64,
    rows: HashMap<String, PaidRow>,
}

impl PaidLedger {
    /// A fresh ledger that starts counting now.
    pub fn started(at: u64) -> Self {
        Self {
            since: at,
            rows: HashMap::new(),
        }
    }

    pub fn get(&self, worker: &str) -> Option<&PaidRow> {
        self.rows.get(worker)
    }

    pub fn workers(&self) -> usize {
        self.rows.len()
    }

    pub fn total_units(&self) -> u64 {
        self.rows
            .values()
            .map(|r| r.units)
            .fold(0u64, |a, b| a.saturating_add(b))
    }

    /// Fold a payout the node has BURIED into the ledger.
    ///
    /// Only ever ADDS: a worker's paid total can never go down, and it moves only
    /// on a node confirmation - never when a payout is merely submitted, and
    /// never when one is only shallowly mined and a reorg could still undo it.
    pub fn credit(&mut self, rec: &PayoutRecord, confirmed_at: u64) {
        for (worker, units) in &rec.rows {
            if *units == 0 {
                continue;
            }
            let row = self.rows.entry(worker.clone()).or_default();
            row.units = row.units.saturating_add(*units);
            row.last_units = *units;
            row.last_hash = rec.hash.clone();
            row.last_at = confirmed_at;
        }
    }

    pub fn to_json(&self) -> Value {
        let mut rows: Vec<(&String, &PaidRow)> = self.rows.iter().collect();
        rows.sort_by(|a, b| a.0.cmp(b.0)); // stable file, stable diffs
        serde_json::json!({
            "since": self.since,
            "rows": rows.iter().map(|(w, r)| serde_json::json!({
                "worker": w,
                "units": r.units,
                "last_units": r.last_units,
                "last_hash": r.last_hash,
                "last_at": r.last_at,
            })).collect::<Vec<_>>(),
        })
    }

    pub fn from_json(v: &Value) -> Self {
        let since = v.get("since").and_then(|x| x.as_u64()).unwrap_or(0);
        let mut rows: HashMap<String, PaidRow> = HashMap::new();
        if let Some(a) = v.get("rows").and_then(|x| x.as_array()) {
            for r in a {
                let Some(w) = r.get("worker").and_then(|x| x.as_str()) else {
                    continue;
                };
                let Some(units) = r.get("units").and_then(|x| x.as_u64()) else {
                    continue; // an unreadable total is not a zero total
                };
                rows.insert(
                    w.to_string(),
                    PaidRow {
                        units,
                        last_units: r.get("last_units").and_then(|x| x.as_u64()).unwrap_or(0),
                        last_hash: r
                            .get("last_hash")
                            .and_then(|x| x.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        last_at: r.get("last_at").and_then(|x| x.as_u64()).unwrap_or(0),
                    },
                );
            }
        }
        Self { since, rows }
    }
}

/// Move a payout the node reports BURIED out of the in-flight list and into the
/// paid ledger, in one step so a unit can never be counted in both.
///
/// Returns the record it credited, or `None` if this pool has no rows for that
/// hash (a payout submitted by an older build, or by a tool that did not record
/// its rows). `None` is why the caller must still drop the hash from the pending
/// ledger: the money moved, this pool just cannot attribute it.
pub fn confirm_payout(
    records: &mut Vec<PayoutRecord>,
    paid: &mut PaidLedger,
    hash: &str,
    confirmed_at: u64,
) -> Option<PayoutRecord> {
    let i = records.iter().position(|r| r.hash == hash)?;
    let rec = records.remove(i);
    paid.credit(&rec, confirmed_at);
    Some(rec)
}

/// Drop a payout the node definitively does NOT hold. Nothing was paid, so
/// nothing is credited: that money is still owed and goes back to `pending`.
pub fn drop_payout(records: &mut Vec<PayoutRecord>, hash: &str) -> Option<PayoutRecord> {
    let i = records.iter().position(|r| r.hash == hash)?;
    Some(records.remove(i))
}

/// The per-transaction rows of every payout this pool has in flight.
pub fn load_payout_records(state_file: &str) -> Vec<PayoutRecord> {
    let Some(j) = read_state_json(state_file) else {
        return Vec::new();
    };
    parse_payout_records(&j)
}

/// Read the in-flight payout rows out of an already-parsed state document.
pub fn parse_payout_records(j: &Value) -> Vec<PayoutRecord> {
    j.get("payouts_inflight")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(PayoutRecord::from_json).collect())
        .unwrap_or_default()
}

/// The confirmed-payout ledger.
pub fn load_paid_ledger(state_file: &str) -> PaidLedger {
    let Some(j) = read_state_json(state_file) else {
        return PaidLedger::default();
    };
    parse_paid_ledger(&j)
}

/// Read the confirmed-payout ledger out of an already-parsed state document.
pub fn parse_paid_ledger(j: &Value) -> PaidLedger {
    j.get("paid").map(PaidLedger::from_json).unwrap_or_default()
}

/// Replace the WHOLE settlement ledger (pending hashes, per-transaction rows and
/// confirmed totals) in the pool state file, preserving every other field.
///
/// One write, because the three move together: a payout leaves the in-flight
/// rows at the same instant it enters the paid totals, and a crash between the
/// two would either lose a payment or count it twice.
pub fn save_settlement_ledger(
    state_file: &str,
    hashes: &[String],
    records: &[PayoutRecord],
    paid: &PaidLedger,
) -> std::io::Result<()> {
    let mut j = read_state_json(state_file).unwrap_or_else(|| serde_json::json!({}));
    j["settle_pending_txs"] = serde_json::json!(hashes);
    j["payouts_inflight"] = Value::Array(records.iter().map(|r| r.to_json()).collect());
    j["paid"] = paid.to_json();
    atomic_write(state_file, j.to_string().as_bytes(), true)
}

/// The lock file guarding one wallet's settlement.
pub fn settle_lock_path(wallet_file: &str) -> String {
    format!("{wallet_file}.settle.lock")
}

/// An exclusive, cross-process claim on one wallet's settlement, held for as
/// long as the value lives. The OS releases it if the holder dies, so a crash
/// can never wedge payouts the way a hand-rolled PID file would.
pub struct SettleLock {
    _file: std::fs::File,
}

/// Take the wallet's settlement lock, or fail if another process holds it.
///
/// The pool server takes this for its whole run and `hbit-pool-payout` takes it for
/// its whole run. Without it the two paths each see the full CONFIRMED balance
/// (a payout sitting in the mempool does not reduce it) and each pays the same
/// PPLNS window - a real double payout of the pool's distributable balance.
pub fn acquire_settle_lock(wallet_file: &str) -> std::io::Result<SettleLock> {
    let path = settle_lock_path(wallet_file);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    // Call it as a trait function so it can never be confused with a same-named
    // inherent method on File.
    fs2::FileExt::try_lock_exclusive(&file)?;
    Ok(SettleLock { _file: file })
}

/// Is this string a payable Hacash address (normal single-key PRIVAKEY)?
/// Workers announce one as `&worker=<address>`; the pool then uses the address
/// itself as the share-accounting key, so payouts need no name->address map.
pub fn is_payout_address(s: &str) -> bool {
    Address::from_readable(s)
        .map(|a| a.is_privakey())
        .unwrap_or(false)
}

/// Environment variable holding the pool wallet passphrase. When it is set the
/// key file is stored ENCRYPTED (Argon2id + AES-256-GCM), so a stolen backup, a
/// VSS/disk snapshot or a decommissioned drive is inert without the passphrase.
pub const WALLET_PASSWORD_ENV: &str = "HBIT_WALLET_PASSWORD";
/// Alternative source for that passphrase: a file holding it, for services that
/// cannot carry secrets in the environment.
pub const WALLET_PASSWORD_FILE_ENV: &str = "HBIT_WALLET_PASSWORD_FILE";

/// The shortest passphrase the pool will protect a real wallet with. Anything
/// shorter is refused rather than silently accepted: it is the only thing
/// standing between a stolen backup and every coin the pool holds.
pub const WALLET_PASSWORD_MIN: usize = 8;

/// Characters in an unencrypted wallet file: a 32-byte private key in hex.
const WALLET_KEY_HEX_LEN: usize = 64;

const WALLET_ENVELOPE_VERSION: u64 = 1;
const WALLET_KDF_M_COST_KB: u32 = 19456;
const WALLET_KDF_T_COST: u32 = 2;
const WALLET_KDF_P_COST: u32 = 1;
/// Upper bounds on the KDF parameters read back from a file, so a tampered
/// envelope cannot turn a startup into an out-of-memory or an endless grind.
const WALLET_KDF_MAX_M_COST_KB: u32 = 256 * 1024;
const WALLET_KDF_MAX_T_COST: u32 = 16;
const WALLET_KDF_MAX_P_COST: u32 = 16;

/// The configured wallet passphrase, or None for the (loudly warned about)
/// plaintext mode.
///
/// Never prompts for anything: a service manager gives this process no terminal,
/// so every input comes from the environment or a file and anything missing is a
/// refusal that names what to set.
fn wallet_password() -> Result<Option<Zeroizing<String>>, String> {
    wallet_password_from(
        std::env::var(WALLET_PASSWORD_ENV).ok(),
        std::env::var(WALLET_PASSWORD_FILE_ENV).ok(),
    )
}

/// The passphrase rule itself, with the two configured values passed in so the
/// whole of it can be exercised without touching this process's environment.
///
/// The variable wins over the file, and an EMPTY variable means "not set" so a
/// service unit that always exports it can still leave it blank. A file that
/// exists but holds nothing is NOT the same as no passphrase at all: read that
/// way, an encrypted wallet would be reported as "no passphrase is configured"
/// to an operator who had configured one, and the fix they were told to apply is
/// the one they had already applied.
///
/// No message built here ever carries the passphrase, or its length: these lines
/// go to a journal that outlives the mistake.
fn wallet_password_from(
    direct: Option<String>,
    file: Option<String>,
) -> Result<Option<Zeroizing<String>>, String> {
    let direct = Zeroizing::new(direct.unwrap_or_default());
    if !direct.is_empty() {
        if direct.len() < WALLET_PASSWORD_MIN {
            return Err(format!(
                "REFUSING to touch the pool wallet: the passphrase in {WALLET_PASSWORD_ENV} is \
                 shorter than {WALLET_PASSWORD_MIN} characters.\n\
                 Nothing was read and nothing was written.\n\
                 What to do: set {WALLET_PASSWORD_ENV} to a longer passphrase, one you have \
                 written down somewhere physical, and start again. If a wallet file already \
                 exists, it must be the passphrase that wallet was created with."
            ));
        }
        return Ok(Some(direct));
    }
    // No passphrase in the environment: fall back to the file, if one is named.
    let Some(file) = file.filter(|f| !f.trim().is_empty()) else {
        return Ok(None);
    };
    if std::path::Path::new(&file).is_dir() {
        return Err(format!(
            "REFUSING to touch the pool wallet: {WALLET_PASSWORD_FILE_ENV} names {file}, which is \
             a directory, not a file.\n\
             What to do: point {WALLET_PASSWORD_FILE_ENV} at the FILE that holds the passphrase \
             and nothing else, or unset it and put the passphrase in {WALLET_PASSWORD_ENV}."
        ));
    }
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => Zeroizing::new(t),
        Err(e) => {
            let why = match e.kind() {
                std::io::ErrorKind::NotFound => "there is no file at that path".to_string(),
                std::io::ErrorKind::PermissionDenied => {
                    "this account is not allowed to read it".to_string()
                }
                std::io::ErrorKind::InvalidData => "it is not text".to_string(),
                _ => format!("the operating system said: {e}"),
            };
            return Err(format!(
                "REFUSING to touch the pool wallet: {WALLET_PASSWORD_FILE_ENV} names {file}, \
                 which cannot be read ({why}).\n\
                 Nothing was read and nothing was written.\n\
                 What to do: point {WALLET_PASSWORD_FILE_ENV} at a readable file holding just the \
                 passphrase, and make sure the account this service runs as can read it. Or unset \
                 it and use {WALLET_PASSWORD_ENV} instead."
            ));
        }
    };
    // Trimmed, exactly as it always has been: a passphrase file written by an
    // editor ends in a newline, and the wallets already on disk were encrypted
    // with the trimmed form.
    let pass = Zeroizing::new(text.trim().to_string());
    if pass.is_empty() {
        return Err(format!(
            "REFUSING to touch the pool wallet: {WALLET_PASSWORD_FILE_ENV} names {file}, which is \
             empty, so there is no passphrase to use.\n\
             Nothing was read and nothing was written. This is NOT being treated as \"no \
             passphrase\": if the wallet is encrypted, running on without one would look like a \
             configuration you never made.\n\
             What to do: put the wallet's passphrase in {file}, or unset \
             {WALLET_PASSWORD_FILE_ENV} and use {WALLET_PASSWORD_ENV}."
        ));
    }
    if pass.len() < WALLET_PASSWORD_MIN {
        return Err(format!(
            "REFUSING to touch the pool wallet: the passphrase in {file} is shorter than \
             {WALLET_PASSWORD_MIN} characters.\n\
             Nothing was read and nothing was written.\n\
             What to do: put a longer passphrase in that file, one you have written down \
             somewhere physical, and start again. If a wallet file already exists, it must be the \
             passphrase that wallet was created with."
        ));
    }
    Ok(Some(pass))
}

fn wallet_derive_key(
    pass: &str,
    salt: &[u8],
    m_cost_kb: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<Zeroizing<[u8; 32]>, String> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let params = Params::new(m_cost_kb, t_cost, p_cost, Some(32))
        .map_err(|e: argon2::Error| e.to_string())?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(pass.as_bytes(), salt, &mut *key)
        .map_err(|e: argon2::Error| e.to_string())?;
    Ok(key)
}

/// Wrap a 64-hex private key in a versioned Argon2id + AES-256-GCM envelope.
fn encrypt_key_hex(key_hex: &str, pass: &str) -> Result<String, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut salt).map_err(|e| e.to_string())?;
    getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
    let key = wallet_derive_key(
        pass,
        &salt,
        WALLET_KDF_M_COST_KB,
        WALLET_KDF_T_COST,
        WALLET_KDF_P_COST,
    )?;
    let cipher = Aes256Gcm::new_from_slice(&*key).map_err(|e| e.to_string())?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), key_hex.as_bytes())
        .map_err(|e: aes_gcm::Error| e.to_string())?;
    Ok(serde_json::json!({
        "hbit_wallet": WALLET_ENVELOPE_VERSION,
        "kdf": "argon2id",
        "kdf_salt": hex::encode(salt),
        "kdf_m_cost_kb": WALLET_KDF_M_COST_KB,
        "kdf_t_cost": WALLET_KDF_T_COST,
        "kdf_p_cost": WALLET_KDF_P_COST,
        "cipher": "aes-256-gcm",
        "cipher_nonce": hex::encode(nonce),
        "ciphertext": hex::encode(ciphertext),
    })
    .to_string())
}

fn envelope_u32(j: &Value, key: &str, default: u32, max: u32) -> Result<u32, String> {
    let v = j.get(key).and_then(|v| v.as_u64()).unwrap_or(default as u64);
    if v == 0 || v > max as u64 {
        return Err(format!("its `{key}` is outside the range this build accepts"));
    }
    Ok(v as u32)
}

/// Why an encrypted wallet file would not open.
///
/// The three are kept apart on purpose, and the ONE distinction that costs money
/// is `Shape` versus `Undecryptable`. Telling an operator their file is corrupt
/// when they merely mistyped the passphrase invites them to restore a backup
/// over the real wallet, or to delete it and let the pool create a fresh empty
/// one, abandoning the funds in the file they still had.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EnvelopeError {
    /// The file is not an envelope this build can read. Decided from the file's
    /// structure alone, BEFORE any passphrase is tried, so it is certain and it
    /// is never a passphrase problem.
    Shape(String),
    /// The authentication tag did not verify. That is a wrong passphrase OR
    /// damaged ciphertext, and AES-GCM cannot tell those apart: it is the same
    /// check that fails either way. Neither can this pool, so neither may the
    /// message it prints.
    Undecryptable,
    /// The tag DID verify, so the passphrase is right and the file is intact,
    /// but what came out of it is not a private key.
    Content(String),
}

/// Unwrap an envelope written by [`encrypt_key_hex`].
///
/// No error carries any part of the passphrase, the ciphertext or the key: the
/// caller turns these into lines that end up in a journal.
fn decrypt_key_hex(body: &str, pass: &str) -> Result<Zeroizing<String>, EnvelopeError> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    let shape = |s: String| EnvelopeError::Shape(s);
    // Everything down to the decrypt itself is a property of the FILE. None of
    // it depends on the passphrase, which is what makes `Shape` certain.
    let j: Value = serde_json::from_str(body)
        .map_err(|_| shape("it is not the JSON this pool writes".to_string()))?;
    let ver = j.get("hbit_wallet").and_then(|v| v.as_u64()).unwrap_or(0);
    if ver != WALLET_ENVELOPE_VERSION {
        return Err(shape(format!(
            "it declares wallet format {ver} and this build reads format {WALLET_ENVELOPE_VERSION}"
        )));
    }
    let hex_field = |k: &str| -> Result<Vec<u8>, EnvelopeError> {
        let s = j
            .get(k)
            .and_then(|v| v.as_str())
            .ok_or_else(|| shape(format!("it is missing the `{k}` field")))?;
        hex::decode(s).map_err(|_| shape(format!("its `{k}` field is not hex")))
    };
    let salt = hex_field("kdf_salt")?;
    let nonce = hex_field("cipher_nonce")?;
    let ciphertext = hex_field("ciphertext")?;
    if nonce.len() != 12 {
        return Err(shape(format!(
            "its nonce is {} bytes and must be 12",
            nonce.len()
        )));
    }
    let key = wallet_derive_key(
        pass,
        &salt,
        envelope_u32(&j, "kdf_m_cost_kb", WALLET_KDF_M_COST_KB, WALLET_KDF_MAX_M_COST_KB)
            .map_err(EnvelopeError::Shape)?,
        envelope_u32(&j, "kdf_t_cost", WALLET_KDF_T_COST, WALLET_KDF_MAX_T_COST)
            .map_err(EnvelopeError::Shape)?,
        envelope_u32(&j, "kdf_p_cost", WALLET_KDF_P_COST, WALLET_KDF_MAX_P_COST)
            .map_err(EnvelopeError::Shape)?,
    )
    .map_err(|e| shape(format!("its key-derivation settings cannot be used here ({e})")))?;
    let cipher = Aes256Gcm::new_from_slice(&*key)
        .map_err(|e| shape(format!("its cipher key could not be set up ({e})")))?;
    // The one check that cannot say WHY it failed.
    let plain = cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
        .map(Zeroizing::new)
        .map_err(|_e: aes_gcm::Error| EnvelopeError::Undecryptable)?;
    let txt = String::from_utf8(plain.to_vec())
        .map(Zeroizing::new)
        .map_err(|_| EnvelopeError::Content("what is inside it is not text".to_string()))?;
    Ok(txt)
}

/// The refusal for an envelope that would not open, said so that an operator
/// cannot mistake one cause for the other.
fn envelope_refusal(path: &str, e: &EnvelopeError) -> String {
    match e {
        EnvelopeError::Shape(why) => format!(
            "REFUSING to open the pool wallet: {path} is not an encrypted wallet file this build \
             can read ({why}).\n\
             This is NOT about your passphrase. The file's structure is checked before any \
             passphrase is tried, so a different passphrase would not change this.\n\
             Nothing has been written to it and no new wallet has been created.\n\
             What to do: check that {path} really is the pool's key file and not another file \
             that ended up at that path, then restore it from your backup. Do not delete it \
             first: nothing here says the key inside is gone."
        ),
        EnvelopeError::Undecryptable => format!(
            "REFUSING to open the pool wallet: {path} did not open.\n\
             This is EITHER the wrong passphrase OR a damaged file, and there is no way to tell \
             which: the check that failed is the same one in both cases. Do not act as though you \
             knew which it was.\n\
             What to do, in this order:\n\
             1. Check the passphrase. It is taken from {WALLET_PASSWORD_ENV} when that is set, \
             otherwise from the file named in {WALLET_PASSWORD_FILE_ENV}. Look for a trailing \
             space, a different keyboard layout, or a service unit still exporting an old value.\n\
             2. Only once you are certain the passphrase is right, restore {path} from your \
             backup.\n\
             DO NOT delete, move or overwrite {path}, and do not let the pool create a new wallet \
             in its place. If the passphrase is simply wrong, that file still holds the key to \
             every coin this pool has mined; a new wallet is a new address, and the old money \
             would be out of reach for good. Nothing has been written to it.",
        ),
        EnvelopeError::Content(why) => format!(
            "REFUSING to open the pool wallet: {path} decrypted correctly, so the passphrase is \
             right and the file is intact, but {why}.\n\
             What to do: this is not a file this pool wrote. Check that {path} is the right file \
             and restore it from your backup. Nothing has been written to it and no new wallet \
             has been created; do not delete it."
        ),
    }
}

/// True the FIRST time this (tag, path) pair comes up in this process. The
/// settlement loop reloads the wallet on every cycle, so once-per-wallet work
/// and warnings must not repeat with it.
fn first_time_for(tag: &str, path: &str) -> bool {
    static SEEN: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    seen.insert(format!("{tag}:{path}"))
}

/// Say plainly what an unencrypted key file costs. Printed once per path per
/// process so it cannot be lost in the settlement loop's output.
fn warn_plaintext_wallet(path: &str) {
    if !first_time_for("plaintext-warned", path) {
        return;
    }
    eprintln!(
        "[wallet] WARNING: {path} holds the pool's private key in PLAINTEXT. Anything that can\n\
         [wallet] read those bytes - a backup, a VSS or disk snapshot, an old drive - can spend\n\
         [wallet] every coin the pool holds. Set {WALLET_PASSWORD_ENV} (or {WALLET_PASSWORD_FILE_ENV})\n\
         [wallet] and restart: the file is then re-written encrypted (Argon2id + AES-256-GCM)."
    );
}

/// Load the pool wallet, or print the refusal and stop.
///
/// Kept for callers that have no way to report a failure of their own; prefer
/// [`try_load_or_create_wallet`], which hands the same text back for the caller
/// to print in its own house style. A refusal exits with status 2, the same
/// status the server uses for a configuration it will not start on, rather than
/// panicking: a panic buries the one line that says what to fix under a
/// backtrace note, in a journal, at 3am, on a machine nobody is logged in to.
pub fn load_or_create_wallet(path: &str) -> Account {
    match try_load_or_create_wallet(path) {
        Ok(acc) => acc,
        Err(why) => {
            eprintln!("{why}");
            std::process::exit(2)
        }
    }
}

/// Load the pool wallet from `path`, creating a fresh random one if, and only
/// if, there is no file there at all. The file holds either a 64-hex secp256k1
/// private key or, when a passphrase is configured, an encrypted envelope. The
/// private key only ever lives in that file: it is never printed or logged, and
/// no refusal below names anything but the FILE that failed.
///
/// Every failure is an `Err` an operator can act on, and NO failure creates,
/// truncates or overwrites `path`. That is the money-critical property: the file
/// on disk is the only copy of the key to the address the pool's income lands
/// in, so a pool that "recovered" from a mistyped passphrase by making a new
/// wallet would abandon every coin in the old one and start owing miners from an
/// address with nothing in it.
pub fn try_load_or_create_wallet(path: &str) -> Result<Account, String> {
    // Resolve the passphrase FIRST, before anything opens the key file, so a
    // passphrase that is missing, empty or too short is refused with the wallet
    // untouched. Resolved once, and passed down, so the settlement loop does not
    // re-read the passphrase file on every cycle either.
    let pass = wallet_password()?;
    load_or_create_wallet_with(path, pass.as_ref().map(|p| p.as_str()))
}

/// The wallet loader with the passphrase already resolved, so the whole of it can
/// be exercised without this process's environment.
fn load_or_create_wallet_with(path: &str, pass: Option<&str>) -> Result<Account, String> {
    // A directory at the wallet path reads back as a different OS error on every
    // platform (and as "permission denied" on Windows), so name it here rather
    // than leave an operator chasing an ACL that is not the problem.
    if std::path::Path::new(path).is_dir() {
        return Err(format!(
            "REFUSING to open the pool wallet: {path} is a directory, not a wallet file.\n\
             Nothing was created inside it and nothing was written.\n\
             What to do: pass the path of the key FILE as <wallet_file>. If you meant a file of \
             that name inside a folder, spell out the whole path, for example \
             {path}{sep}pool-wallet.key",
            sep = std::path::MAIN_SEPARATOR,
        ));
    }
    match std::fs::read_to_string(path) {
        Ok(txt) => {
            let acc = account_from_wallet_file(path, &txt, pass)?;
            // Re-apply and re-verify the owner-only permissions on the LOAD path
            // too, not only at creation: a key that lost its ACL (restored from a
            // backup, copied by hand) must not keep serving funds.
            secure_existing_key_file(path)?;
            println!("pool wallet {} (from {path})", acc.readable());
            Ok(acc)
        }
        // The ONE branch that may write a key: there is no file here at all.
        // Nothing above can reach it, so no failure to read or decrypt an
        // existing wallet can ever fall through into creating a new one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => create_wallet(path, pass),
        // Never generate-and-overwrite on any other error: a locked or
        // transiently-unreadable key file must not be silently replaced.
        Err(e) => Err(unreadable_wallet_refusal(path, &e)),
    }
}

/// The refusal for a wallet file that exists but could not be read.
///
/// Split out from the loader so each cause can be tested, and so each one gets
/// the fix that actually applies to it.
fn unreadable_wallet_refusal(path: &str, e: &std::io::Error) -> String {
    let (why, fix) = match e.kind() {
        std::io::ErrorKind::PermissionDenied => (
            "this account is not allowed to read it".to_string(),
            format!(
                "run the pool as the account that owns {path}, or give that account read access \
                 to it (Windows: icacls, Linux: chown then chmod 600). The pool locks this file \
                 down to one account on purpose, so the usual cause is that the service now runs \
                 as somebody else."
            ),
        ),
        std::io::ErrorKind::InvalidData => (
            "it is not text".to_string(),
            format!(
                "a wallet file is either 64 hex characters or the JSON envelope this pool writes, \
                 and both are plain ASCII. Check that {path} is really the pool's key file and \
                 not some other file that ended up at that path, then restore it from your backup."
            ),
        ),
        _ => (
            format!("the operating system said: {e}"),
            format!(
                "make {path} readable by the account this pool runs as, then start it again. If \
                 the file is on a network or removable drive, check that the drive is mounted."
            ),
        ),
    };
    format!(
        "REFUSING to open the pool wallet: {path} exists but cannot be read ({why}).\n\
         No new wallet has been created in its place, and nothing has been written to it. A new \
         wallet would be a new address, and every coin already mined into this one would be left \
         behind with no way back.\n\
         What to do: {fix}"
    )
}

/// Create the pool's wallet. The ONE path in this file that writes a key.
fn create_wallet(path: &str, pass: Option<&str>) -> Result<Account, String> {
    let acc = new_random_account()?;
    let key_hex = Zeroizing::new(hex::encode(acc.secret_key().serialize()));
    let body = match pass {
        Some(p) => Zeroizing::new(encrypt_key_hex(&key_hex, p).map_err(|e| {
            format!(
                "REFUSING to create the pool wallet: the new key could not be encrypted ({e}).\n\
                 Nothing has been written to {path}.\n\
                 What to do: this is the machine's own crypto or randomness failing, not your \
                 configuration. Try again; if it repeats, the pool must not run here, because a \
                 wallet it cannot protect is a wallet anyone with the disk can empty."
            )
        })?),
        None => Zeroizing::new(key_hex.to_string()),
    };
    if let Err(e) = write_key_file(path, &body) {
        // A key we could not protect must never be left lying around. This runs
        // only here, where a moment ago there was no file at that path, so
        // nothing an operator already had is removed.
        let _ = std::fs::remove_file(path);
        return Err(format!(
            "REFUSING to run with an unprotected pool wallet: {path} could not be written and \
             locked down to this account only ({e}).\n\
             The key that was generated has been discarded, so nothing was left half-written.\n\
             What to do: check that the folder holding {path} exists and that this account may \
             create files in it, then start again. That file is the private key to every coin the \
             pool will hold, so the pool will not settle for leaving it readable by others."
        ));
    }
    println!("CREATED A NEW POOL WALLET -> {path}");
    println!("  address: {}", acc.readable());
    if pass.is_some() {
        println!("  the file is ENCRYPTED with {WALLET_PASSWORD_ENV}.");
        println!("  BACK UP THAT FILE **AND** THAT PASSPHRASE: neither one alone can spend,");
        println!("  and losing either one loses the pool's funds for good.");
    } else {
        println!("  BACK UP THAT FILE. Whoever holds it controls the pool's funds.");
        warn_plaintext_wallet(path);
    }
    Ok(acc)
}

/// A fresh random account, or a refusal.
///
/// Bounded, so a broken generator that keeps returning a value the curve rejects
/// is a refusal rather than a daemon spinning at 100% forever with nothing in the
/// log. A wallet is only ever as good as the randomness behind it, so a failing
/// RNG must stop the pool rather than be retried around.
fn new_random_account() -> Result<Account, String> {
    for _ in 0..64 {
        let mut key = Zeroizing::new([0u8; 32]);
        if let Err(e) = getrandom::fill(&mut *key) {
            return Err(format!(
                "REFUSING to create the pool wallet: this machine's random number generator \
                 failed ({e}). Nothing has been written.\n\
                 What to do: a key made without real randomness could be guessed and the wallet \
                 emptied, so the pool will not make one. Fix the system entropy source (on Linux \
                 that is getrandom on /dev/urandom) and start again."
            ));
        }
        if let Ok(a) = Account::create_by_secret_key_value(*key) {
            return Ok(a);
        }
    }
    Err(
        "REFUSING to create the pool wallet: 64 random keys in a row were all rejected by the \
         curve, which cannot happen with a working random number generator. Nothing has been \
         written.\n\
         What to do: this machine's randomness is broken. Fix it and start again."
            .to_string(),
    )
}

/// Turn 64 hex characters into an Account, and refuse anything else.
///
/// Never `Account::create_by`: when its argument is not exactly 64 hex
/// characters that function falls back to treating the text as a PASSPHRASE and
/// returns the account of its sha2. A key file with one altered character would
/// then load a completely different wallet, and the pool would quietly mine into
/// an address whose key nobody holds while the operator's real funds sat in a
/// file it had stopped using. A refusal is recoverable; a wrong wallet is not.
///
/// The reason it returns is a fragment for the caller's message and never
/// contains any part of the key.
fn account_from_key_hex(key_hex: &str) -> Result<Account, String> {
    let raw = Zeroizing::new(hex::decode(key_hex).map_err(|_| "it is not hexadecimal")?);
    if raw.len() != 32 {
        return Err(format!(
            "it is {} bytes of hex and a private key is 32",
            raw.len()
        ));
    }
    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&raw);
    // The underlying reason is dropped on purpose: nothing derived from key
    // material goes into a message.
    Account::create_by_secret_key_value(*key)
        .map_err(|_| "it is not a private key this curve accepts".to_string())
}

/// Turn the wallet file's contents into an Account, transparently handling both
/// the encrypted envelope and the legacy plaintext-hex form.
///
/// Reads only. Every refusal here leaves `path` exactly as it was found.
fn account_from_wallet_file(path: &str, txt: &str, pass: Option<&str>) -> Result<Account, String> {
    let body = txt.trim();
    if body.is_empty() {
        return Err(format!(
            "REFUSING to open the pool wallet: {path} is empty, so it holds no private key.\n\
             The pool has NOT written a new key into it. It creates a wallet only when there is \
             no file at all, because a new wallet is a new address and any coins mined into the \
             old one would be left behind.\n\
             What to do: if this file is meant to be your pool wallet, restore it from your \
             backup. If this really is a first run and the empty file was made by accident (a \
             shell redirect, an editor), delete the empty file and start the pool again."
        ));
    }
    // A JSON envelope is the encrypted form; anything else is the legacy
    // plaintext key.
    if body.starts_with('{') {
        let Some(pass) = pass else {
            return Err(format!(
                "REFUSING to open the pool wallet: {path} is encrypted and no passphrase is \
                 configured.\n\
                 Nothing has been written to it and no new wallet has been created.\n\
                 What to do: set {WALLET_PASSWORD_ENV} to the passphrase this wallet was created \
                 with, or put that passphrase in a file of its own and name that file in \
                 {WALLET_PASSWORD_FILE_ENV}. Then start again. Do NOT delete or move {path}: it \
                 holds the only key to the address the pool's income is paid into."
            ));
        };
        let key_hex = decrypt_key_hex(body, pass).map_err(|e| envelope_refusal(path, &e))?;
        // The tag verified, so the passphrase is right and the bytes are intact:
        // anything wrong from here is the CONTENT, which is a different thing to
        // tell the operator.
        return account_from_key_hex(key_hex.trim())
            .map_err(|why| envelope_refusal(path, &EnvelopeError::Content(why)));
    }
    if body.len() != WALLET_KEY_HEX_LEN {
        let shape = if body.len() < WALLET_KEY_HEX_LEN {
            "a truncated copy: a transfer that stopped early, or a backup taken while the file was \
             still being written"
        } else {
            "a file with something extra in it: a second key, a note, or two files run together"
        };
        // Said only when it can help. A passphrase being set is exactly when an
        // operator expects to be looking at an encrypted file.
        let hint = if pass.is_some() {
            "\nA passphrase IS configured, so if you expected this wallet to be encrypted: the \
             encrypted form is JSON and starts with `{`. This file does not, so it is not that."
        } else {
            ""
        };
        return Err(format!(
            "REFUSING to open the pool wallet: {path} holds {n} characters. An unencrypted wallet \
             file is exactly {WALLET_KEY_HEX_LEN} (a 32-byte private key written in hex), and an \
             encrypted one is JSON starting with `{{`.\n\
             That looks like {shape}.{hint}\n\
             Nothing has been written to it and no new wallet has been created.\n\
             What to do: restore {path} from your backup. Keep the file you have until the \
             restored one opens: deleting it is the one step that cannot be undone.",
            n = body.len()
        ));
    }
    let acc = account_from_key_hex(body).map_err(|why| {
        format!(
            "REFUSING to open the pool wallet: {path} holds {WALLET_KEY_HEX_LEN} characters, but \
             they are not a private key ({why}).\n\
             Nothing has been written to it and no new wallet has been created, and this pool will \
             not guess: text that is not a key can be turned into SOME wallet, and it would not be \
             yours.\n\
             What to do: restore {path} from your backup. Do not delete the file you have."
        )
    })?;
    // Plaintext on disk: move it into an encrypted envelope as soon as a
    // passphrase is configured, otherwise say plainly what is at risk.
    match pass {
        Some(pass) => migrate_key_file_to_encrypted(path, body, pass)?,
        None => warn_plaintext_wallet(path),
    }
    Ok(acc)
}

/// Re-write a legacy plaintext key file as an encrypted envelope. The envelope
/// is decrypted back and compared BEFORE it replaces the only copy of the key,
/// so a bad envelope can never cost the pool its wallet.
///
/// Failing to encrypt is a warning, not a refusal: the pool ran yesterday with
/// that plaintext file and can run today. Failing to WRITE what was already
/// verified is a refusal, because at that point the state of the file on disk is
/// the thing in doubt.
fn migrate_key_file_to_encrypted(path: &str, key_hex: &str, pass: &str) -> Result<(), String> {
    let body = match encrypt_key_hex(key_hex, pass) {
        Ok(b) => Zeroizing::new(b),
        Err(e) => {
            eprintln!("[wallet] WARNING: could not encrypt {path} ({e}); it stays plaintext.");
            return Ok(());
        }
    };
    match decrypt_key_hex(&body, pass) {
        Ok(back) if back.trim().eq_ignore_ascii_case(key_hex) => {}
        _ => {
            eprintln!("[wallet] WARNING: the encrypted form of {path} did not verify; leaving it as-is.");
            return Ok(());
        }
    }
    // Only now does anything replace the file, and what replaces it has already
    // been decrypted back to the very key that is in it.
    if let Err(e) = write_key_file(path, &body) {
        return Err(format!(
            "REFUSING to run with an unprotected pool wallet: {path} could not be re-written \
             encrypted and owner-only ({e}).\n\
             The key controls ALL pool funds. Either the file is still the plaintext key it was, \
             or it is the encrypted form that was verified before it was written; in both cases \
             the key in it is yours and nothing was lost.\n\
             What to do: check that this account may write in that folder and that no backup tool \
             is holding the file open, then start again. To carry on without encryption for now, \
             unset {WALLET_PASSWORD_ENV} and {WALLET_PASSWORD_FILE_ENV}, knowing that the key is \
             then plaintext on this disk."
        ));
    }
    println!("[wallet] {path} is now ENCRYPTED with {WALLET_PASSWORD_ENV}.");
    println!("[wallet] KEEP THAT PASSPHRASE: without it the file cannot be decrypted and the");
    println!("[wallet] pool's funds are unrecoverable. The previous plaintext copy may still");
    println!("[wallet] exist in backups and snapshots - treat those as sensitive.");
    Ok(())
}

/// Re-apply and verify the key file's owner-only permissions, once per path per
/// process. Settlement reloads the wallet every cycle, and spawning icacls each
/// time would be pointless work that a transient hiccup could turn into a
/// skipped payout.
fn secure_existing_key_file(path: &str) -> Result<(), String> {
    if !first_time_for("secured", path) {
        return Ok(());
    }
    restrict_key_file_permissions(path).map_err(|e| {
        format!(
            "REFUSING to run with an unprotected pool wallet: the owner-only permissions on \
             {path} could not be applied and verified ({e}).\n\
             The contents of the wallet were not changed.\n\
             What to do: that file is the private key to every coin the pool holds, so it must not \
             be readable by other accounts on this machine. Make sure this account owns it \
             (Windows: icacls {path} /inheritance:r /grant:r \"%USERNAME%\":F, Linux: chown then \
             chmod 600), then start again."
        )
    })
}

/// Write the wallet file owner-only via a temp file + atomic rename, so a
/// concurrent reader never sees an empty or half-written key. The file controls
/// ALL pool funds, so securing it is MANDATORY: if the owner-only permissions
/// cannot be applied AND verified this returns Err, and the caller must not keep
/// running with an unprotected key.
///
/// Every failure removes the temp file before it returns. That file holds the
/// private key, and a refusal must not leave a copy of it lying next to the
/// wallet under a name nobody will think to look at.
fn write_key_file(path: &str, body: &str) -> std::io::Result<()> {
    let tmp = format!("{path}.tmp.{}", std::process::id());
    if let Err(e) = fill_key_tmp(&tmp, body) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    restrict_key_file_permissions(path)
}

/// Create the temp file, harden it, then put the key in it.
fn fill_key_tmp(tmp: &str, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(tmp)?;
    // Harden the (still empty) temp file BEFORE the secret reaches it. On
    // Windows it is created with the directory's inherited ACL and NTFS carries
    // that ACL across the rename, so hardening only the final path would leave a
    // window in which the key is readable by other accounts.
    #[cfg(windows)]
    restrict_key_file_permissions(tmp)?;
    writeln!(f, "{body}")?;
    let _ = f.sync_all();
    Ok(())
}

/// Lock the wallet key down to the current user only, and prove it worked.
/// On Windows the default ACL is inherited and readable by other local accounts;
/// without this the key controlling the pool balance is exposed to any local
/// user or process. Every failure is fatal to the caller: "could not secure the
/// private key" is not a warning for a daemon that moves real money.
fn restrict_key_file_permissions(path: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        // Resolve the principal from the process token, NEVER from USERNAME:
        // that variable is empty under a service or a scheduled task, and
        // `/inheritance:r` with no matching `/grant` leaves an EMPTY DACL that
        // locks the pool out of its own wallet on the very next start.
        let (name, sid) = windows_current_principal()?;
        let out = std::process::Command::new("icacls")
            .arg(path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!("*{sid}:F"))
            .output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "icacls failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        // The owner must still be able to READ the key, or every later start
        // dies on "cannot read wallet file".
        drop(std::fs::File::open(path)?);
        windows_verify_owner_only(path, &name, &sid)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(std::io::Error::other(format!(
                "wallet file mode is {mode:o}, expected 600"
            )));
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        return Err(std::io::Error::other(
            "cannot restrict wallet file permissions on this platform",
        ));
    }
    Ok(())
}

/// The current account's `DOMAIN\name` and SID, read from the process token via
/// `whoami /user`. Granting by SID keeps the ACL correct even where the display
/// name is ambiguous, and it never depends on the USERNAME variable.
#[cfg(windows)]
fn windows_current_principal() -> std::io::Result<(String, String)> {
    let out = std::process::Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other("`whoami /user` failed"));
    }
    let txt = String::from_utf8_lossy(&out.stdout);
    let line = txt.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    // CSV is `"DOMAIN\user","S-1-5-..."`; a Windows account name cannot contain
    // a comma, so a plain split is safe.
    let mut cols = line.split(',').map(|c| c.trim().trim_matches('"'));
    let name = cols.next().unwrap_or("").to_string();
    let sid = cols.next().unwrap_or("").to_string();
    if name.is_empty() || !sid.starts_with("S-1-") {
        return Err(std::io::Error::other(
            "could not resolve the current account SID from `whoami /user`",
        ));
    }
    Ok((name, sid))
}

/// Principals it is not meaningful to lock a file against on Windows: LocalSystem
/// and the local Administrators group.
///
/// Excluding these buys nothing. Anything running as either can take ownership of
/// the file, read this process's memory, or load a driver, so a key they cannot
/// read through the DACL is a key they can read another way five seconds later.
/// What the DACL genuinely protects against is OTHER ordinary accounts on a
/// shared machine, and that protection is unaffected by leaving these two in.
///
/// Refusing them instead made the pool fail CLOSED on machines where the OS keeps
/// its own ACE, which for a pool means it will not write its wallet and therefore
/// pays nobody. Availability lost, security unchanged.
#[cfg(windows)]
const WINDOWS_OS_PRINCIPAL_SIDS: [&str; 2] = [
    "S-1-5-18",     // NT AUTHORITY\SYSTEM
    "S-1-5-32-544", // BUILTIN\Administrators
];

/// Resolve an account name printed by icacls to its SID.
///
/// By SID and not by name on purpose: icacls prints LOCALISED names, so matching
/// the strings "NT AUTHORITY\SYSTEM" or "BUILTIN\Administrators" would silently
/// stop working on a German or Greek Windows and start rejecting the very
/// principals this is meant to accept.
#[cfg(windows)]
fn windows_sid_of(principal: &str) -> Option<String> {
    let script = format!(
        "try {{ ([System.Security.Principal.NTAccount]'{}')\
         .Translate([System.Security.Principal.SecurityIdentifier]).Value }} catch {{ '' }}",
        principal.replace('\'', "''")
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    let sid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    sid.starts_with("S-1-").then_some(sid)
}

/// Read the DACL back and refuse to continue if any principal other than the
/// current account, or the operating system itself, is listed. Parsing icacls
/// output is best-effort, so an unreadable listing only warns: the mandatory
/// checks in the caller (icacls exit status plus the file still being readable)
/// already rule out the empty-DACL and silent-failure cases this guards against.
#[cfg(windows)]
fn windows_verify_owner_only(path: &str, name: &str, sid: &str) -> std::io::Result<()> {
    let unverified = |why: &str| {
        eprintln!("[wallet] WARNING: could not verify the ACL of {path} ({why}); check it manually.");
    };
    let Ok(out) = std::process::Command::new("icacls").arg(path).output() else {
        unverified("icacls did not run");
        return Ok(());
    };
    if !out.status.success() {
        unverified("icacls reported an error");
        return Ok(());
    }
    let txt = String::from_utf8_lossy(&out.stdout);
    let mut aces = 0usize;
    for (i, raw) in txt.lines().enumerate() {
        if raw.trim().is_empty() {
            break; // a blank line ends the ACE list
        }
        let entry = if i == 0 {
            // The first line echoes the path we passed, then the first ACE.
            match raw.trim_start().strip_prefix(path) {
                Some(rest) => rest.trim(),
                None => {
                    unverified("unexpected output layout");
                    return Ok(());
                }
            }
        } else {
            raw.trim()
        };
        let Some((principal, _)) = entry.split_once(":(") else {
            continue;
        };
        aces += 1;
        if !principal.eq_ignore_ascii_case(name) && !principal.eq_ignore_ascii_case(sid) {
            if let Some(resolved) = windows_sid_of(principal) {
                if WINDOWS_OS_PRINCIPAL_SIDS.contains(&resolved.as_str()) {
                    // Said out loud rather than passed over silently: the
                    // operator should know exactly who else can read the key.
                    eprintln!(
                        "[wallet] NOTE: {path} is also readable by `{principal}` ({resolved}). \
                         That is the operating system itself and cannot be excluded; anything \
                         able to act as it already controls this machine. No other account can \
                         read the file."
                    );
                    continue;
                }
            }
            return Err(std::io::Error::other(format!(
                "{path} is still accessible to `{principal}`"
            )));
        }
    }
    if aces == 0 {
        unverified("no access entries parsed");
    }
    Ok(())
}

/// The transaction set the NODE has packed for the height a template extends,
/// carried exactly as the node serialized it.
///
/// The whole point of a pool is that it pays its OWN wallet, so it cannot reuse
/// the node's coinbase. It does not have to. `mrklrts` is the node's merkle
/// "prelude modify" list: the sibling hashes on the path from transaction 0 up
/// to the root. Not one of those siblings is derived from transaction 0 (at
/// every level the list takes element 1, which covers original leaves 2 and
/// above), so folding a DIFFERENT coinbase hash through the same list yields
/// exactly the merkle root of "our coinbase + the node's transactions". That is
/// the same arithmetic the node's own `miner_success` performs, and it is what
/// lets this pool keep the node's transactions while still paying itself.
///
/// `bodies` EXCLUDES the node's coinbase: slot 0 of the block is always ours.
#[derive(Clone, Default, Debug)]
pub struct PackedTxs {
    /// Serialized transaction bodies, in the node's packing order, starting at
    /// the node's transaction 1. Kept as raw bytes on purpose: a block is
    /// serialized as `intro || tx0 || tx1 || ...` with the count living in the
    /// intro, so these can be appended verbatim. The pool therefore never has to
    /// decode a transaction, and cannot drop one whose type it does not know.
    pub bodies: Vec<Vec<u8>>,
    /// The node's merkle prelude modify list for this transaction set.
    pub mrklrts: Vec<Hash>,
}

impl PackedTxs {
    /// Transactions in the block this set produces, coinbase included.
    pub fn block_tx_count(&self) -> u32 {
        // A block can hold at most `max_block_txs` (1000 by default), so this
        // cannot truncate in practice; saturate rather than wrap if it ever did.
        self.bodies.len().saturating_add(1).min(u32::MAX as usize) as u32
    }
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
    /// The transactions the node packed for this height, empty when the node
    /// would not tell us. Behind an `Arc` because the pool clones a whole
    /// template on every single share submission while holding its global lock,
    /// and a full block of transaction bodies is up to a megabyte.
    pub txs: Arc<PackedTxs>,
}

/// Read the chain tip and build a template for the next block, computing the
/// next difficulty off-node with the same rule the node will validate against.
///
/// Returns `None` on any transient node/HTTP problem instead of panicking, so a
/// caller holding a lock (the pool server) can skip the cycle and retry rather
/// than poisoning its mutex and taking the whole pool down.
pub fn fetch_template(
    client: &reqwest::blocking::Client,
    base: &str,
    coinbase_addr: &str,
    params: &ChainParams,
) -> Option<Template> {
    let coinbase = Address::from_readable(coinbase_addr).ok()?;
    let latest = get_json(client, &format!("{base}/query/latest"));
    let prev_hei = find_u64(&latest, "height")?;
    let height = prev_hei + 1;
    let (prevhash, prev_ts, prev_diff) = if prev_hei == 0 {
        (mint::genesis::genesis_block_hash(), 1549250700u64, 0u32)
    } else {
        let ij = get_json(client, &format!("{base}/query/block/intro?height={prev_hei}"));
        let ph = find_str(&ij, "hash")?;
        (
            Hash::from_hex(ph.as_bytes()).ok()?,
            find_u64(&ij, "timestamp")?,
            find_u64(&ij, "difficulty")? as u32,
        )
    };
    let timestamp = std::cmp::max(curtimes(), prev_ts.saturating_add(1));
    // ASERT anchors on the activation block's timestamp; only needed above it.
    let anchor_time = if params.needs_anchor(height) {
        let aj = get_json(
            client,
            &format!("{base}/query/block/intro?height={}", params.asert_height),
        );
        find_u64(&aj, "timestamp")?
    } else {
        0
    };
    let (diff_num, target) =
        difficulty::next_difficulty(params, height, timestamp, prev_diff, anchor_time);
    Some(Template {
        height,
        prevhash,
        timestamp,
        difficulty: diff_num,
        target,
        coinbase_addr: coinbase,
        // Callers that mine a block of their own choosing (the spike tools) want
        // exactly the transactions they pass in. `fetch_pool_template` is what
        // attaches the node's packed set for the pool.
        txs: Arc::new(PackedTxs::default()),
    })
}

/// How many sibling hashes `calculate_mrkl_prelude_modify` yields for a block of
/// `n` transactions: one per merkle level above the leaves, i.e. ceil(log2(n)).
///
/// Used as a structural cross-check on what the node sent. A modify list that
/// does not match the transaction count builds a merkle root the node cannot
/// reproduce, and the block is thrown away with its entire reward.
pub fn mrkl_modify_len_for(n: usize) -> usize {
    let mut levels = 0usize;
    let mut width = n;
    while width > 1 {
        width = width.div_ceil(2);
        levels += 1;
    }
    levels
}

/// Read a `/query/miner/pending?detail&transaction&stuff` reply into the packed
/// transaction set for `height` on top of `prevhash`.
///
/// Returns `Err` with an operator-readable reason on ANY doubt. The caller then
/// mines a coinbase-only block, which is always valid, rather than gambling a
/// whole block reward on a transaction set that may not belong to this height.
pub fn parse_node_packed_txs(
    j: &Value,
    height: u64,
    prevhash: &Hash,
) -> Result<PackedTxs, String> {
    if find_u64(j, "ret") != Some(0) {
        let err = find_str(j, "err")
            .or_else(|| find_str(j, "http_error"))
            .unwrap_or_else(|| j.to_string());
        return Err(format!("the node would not serve its pending block ({err})"));
    }
    let Some(node_hei) = find_u64(j, "height") else {
        return Err("the node's pending block carries no height".to_string());
    };
    if node_hei != height {
        return Err(format!(
            "the node is packing height {node_hei} while this template extends {height}"
        ));
    }
    // Height alone is not enough: after a same-height reorg the node repacks on a
    // DIFFERENT parent, and those transactions were validated against a state
    // this template does not extend.
    let Some(node_prev) = find_str(j, "prevhash") else {
        return Err("the node's pending block carries no prevhash".to_string());
    };
    let Ok(node_prev) = Hash::from_hex(node_prev.as_bytes()) else {
        return Err("the node's pending block carries an unreadable prevhash".to_string());
    };
    if node_prev != *prevhash {
        return Err(format!(
            "the node is packing on parent {} while this template extends {}",
            node_prev.to_hex(),
            prevhash.to_hex()
        ));
    }
    let Some(bodies_json) = find_value(j, "transaction_body_list").and_then(|v| v.as_array()) else {
        return Err("the node's pending block carries no transaction_body_list".to_string());
    };
    let Some(mkrl_json) = find_value(j, "mkrl_modify_list").and_then(|v| v.as_array()) else {
        return Err("the node's pending block carries no mkrl_modify_list".to_string());
    };
    if bodies_json.is_empty() {
        return Err("the node's pending block holds no transactions at all".to_string());
    }
    // Slot 0 is the node's own coinbase, paying the NODE's reward address. It is
    // dropped: this pool must pay its own wallet, and the modify list below is
    // exactly what makes that substitution legal.
    let mut bodies = Vec::with_capacity(bodies_json.len() - 1);
    for item in bodies_json.iter().skip(1) {
        let Some(text) = item.as_str() else {
            return Err("a transaction body in the node's pending block is not a string".to_string());
        };
        let Ok(raw) = hex::decode(text) else {
            return Err("a transaction body in the node's pending block is not hex".to_string());
        };
        if raw.is_empty() {
            return Err("the node's pending block carries an empty transaction body".to_string());
        }
        bodies.push(raw);
    }
    let mut mrklrts = Vec::with_capacity(mkrl_json.len());
    for item in mkrl_json {
        let Some(text) = item.as_str() else {
            return Err("a mkrl_modify_list entry is not a string".to_string());
        };
        let Ok(hx) = Hash::from_hex(text.as_bytes()) else {
            return Err("a mkrl_modify_list entry is not a 32-byte hash".to_string());
        };
        mrklrts.push(hx);
    }
    let txn = bodies.len() + 1;
    let want = mrkl_modify_len_for(txn);
    if mrklrts.len() != want {
        return Err(format!(
            "the node sent {} merkle sibling(s) for {txn} transaction(s) but {want} are needed; \
             mining on that would build a merkle root the node cannot reproduce",
            mrklrts.len()
        ));
    }
    Ok(PackedTxs { bodies, mrklrts })
}

/// Ask the node for the transaction set it packed for `height` on `prevhash`.
pub fn fetch_node_packed_txs(
    client: &reqwest::blocking::Client,
    base: &str,
    height: u64,
    prevhash: &Hash,
) -> Result<PackedTxs, String> {
    let j = get_json(
        client,
        &format!("{base}/query/miner/pending?detail=true&transaction=true&stuff=true"),
    );
    parse_node_packed_txs(&j, height, prevhash)
}

/// May `current`'s packed transactions simply be carried over to `fresh`?
///
/// Only when both describe the same block: the same height on the same parent.
/// The node repacks only when the tip moves, and the pool likewise keeps one
/// template for the life of a height (swapping it mid-height would change the
/// merkle root under every worker), so carrying the set over is exactly what the
/// pool would mine anyway - and it saves re-downloading a block's worth of
/// transaction bodies every couple of seconds. A set that is empty is always
/// re-asked for, so a node that starts serving its mempool is picked up and the
/// "mining without transactions" reason stays current.
fn packed_txs_still_apply(current: Option<&Template>, fresh: &Template) -> Option<Arc<PackedTxs>> {
    let cur = current?;
    if cur.txs.bodies.is_empty() || cur.height != fresh.height || cur.prevhash != fresh.prevhash {
        return None;
    }
    Some(cur.txs.clone())
}

/// The template a POOL mines on: the chain tip plus the transaction set the node
/// packed for that same height.
///
/// Pass the template the pool is currently serving as `current` so an unchanged
/// tip does not re-download the node's whole transaction set.
///
/// The second element is `None` when the node's transactions really are in the
/// template, and otherwise an operator-readable reason why they are not. The
/// caller MUST surface it. A coinbase-only block earns no transaction fees, and
/// on a chain where this pool is the only miner it also means the pool's OWN
/// payout transactions can never confirm: they sit in the node's mempool while
/// block after block is mined carrying nothing.
pub fn fetch_pool_template(
    client: &reqwest::blocking::Client,
    base: &str,
    coinbase_addr: &str,
    params: &ChainParams,
    current: Option<&Template>,
) -> Option<(Template, Option<String>)> {
    let mut tpl = fetch_template(client, base, coinbase_addr, params)?;
    if let Some(txs) = packed_txs_still_apply(current, &tpl) {
        tpl.txs = txs;
        return Some((tpl, None));
    }
    match fetch_node_packed_txs(client, base, tpl.height, &tpl.prevhash) {
        Ok(txs) => {
            tpl.txs = Arc::new(txs);
            Some((tpl, None))
        }
        // Never refuse to mine over this: a coinbase-only block is still a valid
        // block and still pays the round. Mine it, and say loudly why it is thin.
        Err(why) => Some((tpl, Some(why))),
    }
}

/// Prove the off-node difficulty rule agrees with the node BEFORE mining on it.
///
/// A chain selector carries only a name, while the node reads
/// `difficulty_adjust_blocks` and `each_block_target_time` from its own config
/// file. Mainnet fixes both by consensus, but a testnet configured with any
/// other pair recomputes a different difficulty for every block and rejects
/// everything the pool mines - silently, forever. So recompute the difficulty of
/// the node's OWN tip from its stored data and compare against what it stored:
/// an exact match is the only proof that the parameters in force here are the
/// ones the node validates with.
pub fn verify_chain_params(
    client: &reqwest::blocking::Client,
    base: &str,
    params: &ChainParams,
) -> Result<(), String> {
    let latest = get_json(client, &format!("{base}/query/latest"));
    let Some(tip) = find_u64(&latest, "height") else {
        return Err("could not read the chain tip from the node".to_string());
    };
    if tip == 0 {
        return Ok(()); // empty chain: the node has stored nothing to compare to
    }
    if tip > params.bootstrap_max && tip < params.asert_height {
        return Err(format!(
            "the node's tip {tip} is in the pre-ASERT range this pool does not implement \
             (ASERT anchors at {}); a pool only mines at the tip, so wait for the node to \
             sync past that height - or pass the chain the node is really running",
            params.asert_height
        ));
    }
    let intro = |h: u64| get_json(client, &format!("{base}/query/block/intro?height={h}"));
    let b = intro(tip);
    let (Some(ts), Some(stored)) = (find_u64(&b, "timestamp"), find_u64(&b, "difficulty")) else {
        return Err(format!("could not read block {tip} from the node"));
    };
    let prev_diff = if tip > 1 {
        match find_u64(&intro(tip - 1), "difficulty") {
            Some(d) => d as u32,
            None => return Err(format!("could not read block {} from the node", tip - 1)),
        }
    } else {
        0
    };
    let anchor_time = if params.needs_anchor(tip) {
        match find_u64(&intro(params.asert_height), "timestamp") {
            Some(t) => t,
            None => {
                return Err(format!(
                    "could not read the ASERT anchor block {} from the node",
                    params.asert_height
                ));
            }
        }
    } else {
        0
    };
    let (ours, _) = difficulty::next_difficulty(params, tip, ts, prev_diff, anchor_time);
    if ours as u64 != stored {
        return Err(format!(
            "difficulty rule mismatch at the node's own tip {tip}: this pool computes {ours}, \
             the chain stored {stored}. Every block mined against these parameters would be \
             rejected. For a testnet, pass the node's real config as \
             `testnet:<difficulty_adjust_blocks>:<each_block_target_time>`"
        ));
    }
    Ok(())
}

/// The 16-byte message stamped into every block this pool mines, tagging it as
/// HBIT. Fixed16 needs exactly 16 bytes, so the tag is space-padded.
pub fn coinbase_message() -> Fixed16 {
    Fixed16::from_readable(b"HBIT pool       ").unwrap_or_default()
}

/// The template's coinbase carrying `extranonce` in its miner_nonce field.
pub fn coinbase_with_extranonce(tpl: &Template, extranonce: &[u8; 32]) -> mint::TransactionCoinbase {
    let mut cb =
        mint::create_coinbase_tx(tpl.height, coinbase_message(), tpl.coinbase_addr.clone());
    let en = Hash::from_hex(hex::encode(extranonce).as_bytes()).expect("extranonce");
    cb.extend = mint::CoinbaseExtend::must(mint::CoinbaseExtendDataV1 {
        miner_nonce: en,
        witness_count: Uint1::from(0),
    });
    cb
}

fn build_intro(tpl: &Template, cb: &mint::TransactionCoinbase, nonce: u32) -> BlockIntro {
    // Keep the node's packed transactions and swap ONLY slot 0 for our coinbase,
    // recomputing the merkle root the way the node's own `miner_success` does:
    // fold our coinbase hash through the node's prelude modify list. With an
    // empty list (no packed transactions) this reduces to the coinbase hash,
    // which is exactly `calculate_mrklroot(&vec![cb.hash_with_fee()])`.
    //
    // The node validates `transaction_count == transaction_hash_list().len()`
    // and `mrklroot == calculate_mrklroot(transaction_hash_list(true))`, so both
    // fields have to move together with the body `assemble_block` writes.
    BlockIntro {
        head: BlockHead {
            version: Uint1::from(1),
            height: BlockHeight::from(tpl.height),
            timestamp: Timestamp::from(tpl.timestamp),
            prevhash: tpl.prevhash.clone(),
            mrklroot: calculate_mrkl_prelude_update(cb.hash_with_fee(), &tpl.txs.mrklrts),
            transaction_count: Uint4::from(tpl.txs.block_tx_count()),
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

/// Serialized full block for a winning (extranonce, nonce): OUR coinbase in slot
/// 0 followed by every transaction the node packed for this height.
///
/// A serialized `BlockV1` is exactly `intro || tx0 || tx1 || ...`, with the
/// count living in the intro's `transaction_count` and never in the body, so
/// the node's transaction bodies are appended verbatim. That is deliberate: the
/// pool has no transaction codec registry installed, and decoding would silently
/// drop (or refuse) any transaction type it did not itself know about, which is
/// exactly the sort of thing that turns into a rejected block.
pub fn assemble_block(tpl: &Template, cb: &mint::TransactionCoinbase, nonce: u32) -> Vec<u8> {
    let mut out = intro_bytes(tpl, cb, nonce);
    out.extend_from_slice(&cb.serialize());
    for body in &tpl.txs.bodies {
        out.extend_from_slice(body);
    }
    out
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
    let Some(tpl) = fetch_template(client, base, coinbase_addr, params) else {
        return (
            0,
            "{\"ok\":false,\"err\":\"could not fetch a template from the node\"}".to_string(),
        );
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    use protocol::action::HacToTrs;

    /// A scratch path under the system temp dir, unique per test and per run.
    fn tmp_path(tag: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!("hbit-pool-test-{}-{tag}", std::process::id()));
        p.to_string_lossy().to_string()
    }

    /// The predicate the settlement guard used before this fix: a hash was kept
    /// only while the node reported it in the mempool. Every test below shows a
    /// case where it says "resolved" and the payout is in fact still undoable.
    fn old_guard_kept_the_hash(j: &Value) -> bool {
        find_u64(j, "ret") == Some(0)
            && j.get("pending").and_then(|v| v.as_bool()).unwrap_or(false)
    }

    #[test]
    fn payout_tx_confirmed_at_depth_one_is_not_final() {
        // The old guard cleared the ledger the moment a payout left the mempool,
        // so a reorg of the payout block reopened the double-pay window.
        let shallow = serde_json::json!({"ret":0,"hash":"aa","confirm":1});
        assert!(!old_guard_kept_the_hash(&shallow));
        assert_eq!(classify_payout_tx(&shallow), PayoutTxState::Confirming(1));
        let deep = serde_json::json!({"ret":0,"hash":"aa","confirm":PAYOUT_MATURITY_DEPTH});
        assert_eq!(
            classify_payout_tx(&deep),
            PayoutTxState::Buried(PAYOUT_MATURITY_DEPTH)
        );
    }

    #[test]
    fn payout_tx_state_fails_safe_on_an_inconclusive_answer() {
        // get_json turns any transport failure into this object. Reading it as
        // "the payout is gone" is what let a timed-out query clear the guard.
        let http = serde_json::json!({"http_error":"operation timed out"});
        assert!(!old_guard_kept_the_hash(&http));
        assert_eq!(classify_payout_tx(&http), PayoutTxState::Unknown);
        let garbage = Value::String("<html>502</html>".to_string());
        assert_eq!(classify_payout_tx(&garbage), PayoutTxState::Unknown);
        // ret=0 with neither `pending` nor `confirm` is a shape we do not know.
        let odd = serde_json::json!({"ret":0,"hash":"aa"});
        assert_eq!(classify_payout_tx(&odd), PayoutTxState::Unknown);
        // Definitive answers stay definitive.
        assert_eq!(
            classify_payout_tx(&serde_json::json!({"ret":0,"pending":true})),
            PayoutTxState::Pending
        );
        assert_eq!(
            classify_payout_tx(&serde_json::json!({"ret":1,"err":"transaction not found"})),
            PayoutTxState::Gone
        );
    }

    #[test]
    fn an_implausible_balance_is_refused_instead_of_saturating() {
        // "1:280" used to saturate to u64::MAX, which distributable_units then
        // handed to split_payout as a payout plan for the whole u64 range.
        assert_eq!(balance_units("1:280"), None);
        assert_eq!(balance_units("99999999999999999999:248"), None);
        assert_eq!(balance_units("not-a-balance"), None);
        assert_eq!(balance_units("x:247"), None);
        // Real answers still value identically to before.
        assert_eq!(balance_units("49:246"), Some(4)); // 4.9 HAC floors to 4
        assert_eq!(balance_units("1:248"), Some(10)); // 1 HAC = 10 units
        assert_eq!(balance_units("1:247"), Some(1));
        assert_eq!(balance_units("5:240"), Some(0)); // finer than 0.1 HAC
        // The node omits the field for an address holding nothing: not an error.
        assert_eq!(balance_units(""), Some(0));
        // Anything past the plausibility ceiling is a corrupt answer, not money.
        assert_eq!(balance_units(&format!("{MAX_PLAUSIBLE_UNITS}:247")), Some(MAX_PLAUSIBLE_UNITS));
        assert_eq!(balance_units(&format!("{}:247", MAX_PLAUSIBLE_UNITS + 1)), None);
    }

    #[test]
    fn distributable_holds_back_immature_income_and_never_wraps() {
        // 100 units in the wallet, 60 of them from a block that is not yet
        // buried: only the matured 40 minus the reserve may be paid.
        assert_eq!(distributable_units(100, 60, 5), Some(35));
        // Nothing matured beyond the reserve -> do not settle at all.
        assert_eq!(distributable_units(100, 96, 5), None);
        assert_eq!(distributable_units(100, 100, 0), None);
        // A nonsense reserve must fail the guard, not wrap it open.
        assert_eq!(distributable_units(100, 0, u64::MAX), None);
        assert_eq!(distributable_units(100, 0, 5), Some(95));
    }

    #[test]
    fn only_the_nodes_own_view_admits_a_payout() {
        // `/submit/transaction` validates the tx synchronously and then does the
        // mempool insert on a background task whose result it discards, so ret=0
        // is NOT evidence the node took the transaction. Every caller must ask
        // the node what it holds, and must treat "no answer" as unresolved
        // rather than as either outcome.
        let j = |s: &str| serde_json::from_str::<Value>(s).expect("json");
        assert_eq!(admission_of(&j(r#"{"ret":0,"pending":true}"#)), Admission::Held);
        assert_eq!(
            admission_of(&j(&format!(r#"{{"ret":0,"confirm":{PAYOUT_MATURITY_DEPTH}}}"#))),
            Admission::Held
        );
        // Mined but shallow is still the node holding it.
        assert_eq!(admission_of(&j(r#"{"ret":0,"confirm":1}"#)), Admission::Held);
        // The node answered: it does not know this transaction.
        assert_eq!(
            admission_of(&j(r#"{"ret":1,"err":"transaction not found"}"#)),
            Admission::Missing
        );
        // Not answers: never resolve a payout on these.
        assert_eq!(
            admission_of(&j(r#"{"http_error":"connection refused"}"#)),
            Admission::Unresolved
        );
        assert_eq!(admission_of(&j(r#"{"ret":0}"#)), Admission::Unresolved);
        assert_eq!(admission_of(&Value::String("<html>502</html>".into())), Admission::Unresolved);
    }

    #[test]
    fn block_reward_units_are_tenths_of_a_hac() {
        // Height 1 pays 1 HAC = 10 units of 0.1 HAC; the schedule steps at 100k.
        assert_eq!(block_reward_units(1), 10);
        assert_eq!(
            block_reward_units(2_500_000),
            mint::genesis::block_reward_number(2_500_000) as u64 * 10
        );
    }

    #[test]
    fn pending_ledger_is_shared_and_preserves_the_rest_of_the_state() {
        let path = tmp_path("ledger.state.json");
        let _ = std::fs::remove_file(&path);
        // The server owns this file and keeps accounting in it.
        atomic_write(
            &path,
            serde_json::json!({
                "order": ["a", "b"],
                "accepted": 7,
                "immature": [{"height": 9, "hash": "ab", "units": 30}],
                "settle_pending_txs": ["deadbeef"],
            })
            .to_string()
            .as_bytes(),
            true,
        )
        .expect("write state");
        assert_eq!(load_pending_payout_txs(&path), vec!["deadbeef".to_string()]);
        assert_eq!(load_immature_units(&path), 30);
        // The payout tool writes the SAME ledger, without losing the accounting.
        save_pending_payout_txs(&path, &["cafe".to_string()]).expect("save ledger");
        assert_eq!(load_pending_payout_txs(&path), vec!["cafe".to_string()]);
        let j: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(j["accepted"].as_u64(), Some(7));
        assert_eq!(j["order"].as_array().map(|a| a.len()), Some(2));
        assert_eq!(load_immature_units(&path), 30);
        // The share window is readable from the same file, so the payout tool can
        // still settle correctly with the pool server stopped.
        assert_eq!(
            load_pplns_counts(&path),
            vec![("a".to_string(), 1u64), ("b".to_string(), 1u64)]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_per_worker_settlement_ledger_survives_the_state_file() {
        // A miner's "total paid" is only as good as this file. The pool server
        // and hbit-pool-payout both write it, so it has to round-trip through both
        // without losing anyone's money or the rest of the accounting.
        let path = tmp_path("earnings.state.json");
        let _ = std::fs::remove_file(&path);
        atomic_write(
            &path,
            serde_json::json!({"order": ["a"], "accepted": 3})
                .to_string()
                .as_bytes(),
            true,
        )
        .expect("write state");
        // A file written before this ledger existed reads as an empty one, not as
        // a corrupt one: nobody has been paid yet, which is the truth.
        assert!(load_payout_records(&path).is_empty());
        assert_eq!(load_paid_ledger(&path), PaidLedger::default());

        let mut records = vec![PayoutRecord {
            hash: "aa11".to_string(),
            at: 1_100,
            node_holds: true,
            rows: vec![("w1".to_string(), 12), ("w2".to_string(), 3)],
        }];
        let mut paid = PaidLedger::started(1_000);
        save_settlement_ledger(&path, &["aa11".to_string()], &records, &paid).expect("save");

        // Everything else in the file is untouched.
        let j: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(j["accepted"].as_u64(), Some(3));
        assert_eq!(load_pending_payout_txs(&path), vec!["aa11".to_string()]);
        assert_eq!(load_payout_records(&path), records);

        // Confirm it, save again, and reload: the money is in the paid ledger and
        // out of the in-flight rows, in both memory and on disk.
        confirm_payout(&mut records, &mut paid, "aa11", 1_200).expect("credited");
        save_settlement_ledger(&path, &[], &records, &paid).expect("save");
        assert!(load_payout_records(&path).is_empty());
        let back = load_paid_ledger(&path);
        assert_eq!(back, paid);
        assert_eq!(back.get("w1").expect("w1").units, 12);
        assert_eq!(back.get("w2").expect("w2").units, 3);
        assert_eq!(back.get("w1").expect("w1").last_hash, "aa11");
        assert_eq!(back.get("w1").expect("w1").last_at, 1_200);
        assert_eq!(back.total_units(), 15);
        assert_eq!(back.since, 1_000);
        assert!(back.get("never-paid").is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pool_money_terms_are_stated_in_the_chains_own_amount() {
        // Never a float and never a decimal this pool rounded itself.
        assert_eq!(payout_amount(0).to_fin_string(), "0:0");
        assert_eq!(payout_amount(1).to_fin_string(), "1:247"); // 0.1 HAC
        assert_eq!(payout_amount(10).to_fin_string(), "1:248"); // 1 HAC
        assert_eq!(payout_amount(35).to_fin_string(), "35:247");
        assert_eq!(chunk_tx_fee().to_fin_string(), "1:246"); // 0.01 HAC
        // The reserve and the minimum payout are the ones the settlement uses.
        assert_eq!(SETTLE_RESERVE_UNITS, 5); // 0.5 HAC
        assert_eq!(PAYOUT_DUST_UNITS, 1); // 0.1 HAC
        assert_eq!(POOL_FEE_UNITS, 0); // no pool fee
        // And the same value the chain would build from its own text form.
        assert_eq!(
            payout_amount(35),
            Amount::from("35:247").expect("chain amount")
        );
    }

    #[test]
    fn settle_lock_is_exclusive_across_holders() {
        let wallet = tmp_path("lock-wallet.key");
        let lock = settle_lock_path(&wallet);
        let _ = std::fs::remove_file(&lock);
        let held = acquire_settle_lock(&wallet).expect("first holder takes the lock");
        assert!(
            acquire_settle_lock(&wallet).is_err(),
            "a second settler must be refused while the first one holds the wallet"
        );
        drop(held);
        let again = acquire_settle_lock(&wallet).expect("lock is free once released");
        drop(again);
        let _ = std::fs::remove_file(&lock);
    }

    #[test]
    fn a_written_key_file_is_locked_down_and_still_readable_by_us() {
        // Securing the key is now fatal-on-failure, so this must actually work on
        // the host: a broken principal lookup or an empty DACL would stop the
        // pool starting at all (and on Windows an empty DACL denies even the
        // owner a read, which is exactly the self-brick this guards against).
        let path = tmp_path("acl-wallet.key");
        let _ = std::fs::remove_file(&path);
        write_key_file(&path, "deadbeef").expect("write and secure the key file");
        assert_eq!(
            std::fs::read_to_string(&path).expect("owner can still read").trim(),
            "deadbeef"
        );
        // Re-applying on the load path must be idempotent, not a failure.
        restrict_key_file_permissions(&path).expect("re-applying the permissions is idempotent");
        assert!(std::fs::File::open(&path).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wallet_envelope_round_trips_and_rejects_a_wrong_passphrase() {
        let key_hex = "11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff";
        let body = encrypt_key_hex(key_hex, "correct horse battery").expect("encrypt");
        assert!(!body.contains(key_hex), "the key must not survive in the clear");
        let back = decrypt_key_hex(&body, "correct horse battery").expect("decrypt");
        assert_eq!(back.trim(), key_hex);
        assert!(decrypt_key_hex(&body, "wrong passphrase").is_err());
    }

    /* ---- the pool must mine the node's transactions, not a lone coinbase ---- */

    /// A distinct 32-byte hash per index, so a merkle test can talk about leaves
    /// without needing real transactions.
    fn leaf(i: u8) -> Hash {
        Hash::from([i.wrapping_mul(37).wrapping_add(11); 32])
    }

    /// A template with no header fields that matter to the merkle arithmetic.
    fn tpl_with(txs: PackedTxs) -> Template {
        Template {
            height: 1234,
            prevhash: leaf(9),
            timestamp: 1_700_000_000,
            difficulty: LOWEST_DIFFICULTY,
            target: [0xff; 32],
            coinbase_addr: Address::default(),
            txs: Arc::new(txs),
        }
    }

    /// A real, serializable transaction to sit behind the pool's coinbase.
    fn a_transaction(nth: u64) -> TransactionType2 {
        let mut tx = TransactionType2::new_by(
            Address::default(),
            Amount::from("1:246").expect("fee"),
            1_700_000_000 + nth,
        );
        let mut act = HacToTrs::new();
        act.to = AddrOrPtr::from_addr(Address::default());
        act.hacash = Amount::from(&format!("{}:247", nth + 1)).expect("amount");
        tx.push_action(Box::new(act)).expect("push action");
        tx
    }

    #[test]
    fn the_nodes_merkle_sibling_list_survives_a_coinbase_swap() {
        // This is the whole basis of the fix. `calculate_mrkl_prelude_modify`
        // collects, at every level, the sibling of the leftmost element - and no
        // sibling is ever derived from leaf 0. So the list the node computes for
        // ITS block is equally valid for a block with the SAME transactions and a
        // DIFFERENT transaction 0, which is what lets the pool pay its own wallet
        // and still carry the node's transactions.
        for n in 1..=9usize {
            let node_list: Vec<Hash> = (0..n).map(|i| leaf(i as u8)).collect();
            let modify = calculate_mrkl_prelude_modify(&node_list);
            assert_eq!(
                modify.len(),
                mrkl_modify_len_for(n),
                "sibling count for {n} transaction(s)"
            );
            // The identity the node relies on: folding leaf 0 back through the
            // list reproduces the node's own root.
            assert_eq!(
                calculate_mrkl_prelude_update(node_list[0], &modify),
                calculate_mrklroot(&node_list),
                "prelude update must reproduce the plain root for {n} transaction(s)"
            );
            // The identity the POOL relies on: the same list, a different slot 0.
            let ours = Hash::from([0xa7u8; 32]);
            let mut swapped = node_list.clone();
            swapped[0] = ours;
            assert_eq!(
                calculate_mrkl_prelude_update(ours, &modify),
                calculate_mrklroot(&swapped),
                "a swapped coinbase must still land on the node's root for {n} transaction(s)"
            );
        }
    }

    #[test]
    fn an_assembled_block_is_byte_identical_to_the_one_the_node_verifies() {
        // Regression for the defect: every block this pool produced carried
        // `txs 0`, so it collected no fees and - on a chain where the pool is the
        // only miner - its own payouts could never confirm. The block below must
        // now come out byte-for-byte the same as the BlockV1 the node's verifier
        // reconstructs from those bytes, merkle root and tx count included.
        let cb = mint::create_coinbase_tx(1234, coinbase_message(), Address::default());
        let txs: Vec<TransactionType2> = (0..3).map(a_transaction).collect();

        // What the node would build and then verify: hashes WITH fee, in order.
        let mut hashes: Vec<Hash> = vec![cb.hash_with_fee()];
        let mut want_txs = DynVecTransaction::default();
        want_txs.push(Box::new(cb.clone())).expect("push coinbase");
        for tx in &txs {
            hashes.push(tx.hash_with_fee());
            want_txs.push(Box::new(tx.clone())).expect("push tx");
        }
        let want = BlockV1 {
            intro: BlockIntro {
                head: BlockHead {
                    version: Uint1::from(1),
                    height: BlockHeight::from(1234u64),
                    timestamp: Timestamp::from(1_700_000_000u64),
                    prevhash: leaf(9),
                    mrklroot: calculate_mrklroot(&hashes),
                    transaction_count: Uint4::from(4u32),
                },
                meta: BlockMeta {
                    nonce: Uint4::from(77u32),
                    difficulty: Uint4::from(LOWEST_DIFFICULTY),
                    witness_stage: Fixed2::default(),
                },
            },
            transactions: want_txs,
        };

        // What the pool builds from ONLY the node's raw bodies and sibling list.
        let tpl = tpl_with(PackedTxs {
            bodies: txs.iter().map(|t| t.serialize()).collect(),
            mrklrts: calculate_mrkl_prelude_modify(&hashes),
        });
        let got = assemble_block(&tpl, &cb, 77);
        assert_eq!(
            hex::encode(&got),
            hex::encode(want.serialize()),
            "the pool's block must be exactly the block the node verifies"
        );
        let intro = build_intro(&tpl, &cb, 77);
        assert_eq!(*intro.head.transaction_count, 4, "coinbase plus 3 packed txs");
        assert_eq!(
            intro.head.mrklroot,
            calculate_mrklroot(&hashes),
            "the merkle root must match the node's own rule"
        );
    }

    #[test]
    fn a_template_with_no_packed_txs_still_builds_the_old_coinbase_only_block() {
        // The fallback path (node will not serve its pending block) must keep
        // producing exactly what this pool produced before, or an operator whose
        // node has the miner disabled would stop mining altogether.
        let cb = mint::create_coinbase_tx(1234, coinbase_message(), Address::default());
        let tpl = tpl_with(PackedTxs::default());
        let intro = build_intro(&tpl, &cb, 5);
        assert_eq!(*intro.head.transaction_count, 1);
        assert_eq!(intro.head.mrklroot, calculate_mrklroot(&vec![cb.hash_with_fee()]));
        let mut only_cb = DynVecTransaction::default();
        only_cb.push(Box::new(cb.clone())).expect("push coinbase");
        let want = BlockV1 {
            intro,
            transactions: only_cb,
        };
        assert_eq!(assemble_block(&tpl, &cb, 5), want.serialize());
    }

    #[test]
    fn merkle_sibling_counts_are_one_per_level() {
        assert_eq!(mrkl_modify_len_for(1), 0);
        assert_eq!(mrkl_modify_len_for(2), 1);
        assert_eq!(mrkl_modify_len_for(3), 2);
        assert_eq!(mrkl_modify_len_for(4), 2);
        assert_eq!(mrkl_modify_len_for(5), 3);
        assert_eq!(mrkl_modify_len_for(8), 3);
        assert_eq!(mrkl_modify_len_for(9), 4);
        assert_eq!(mrkl_modify_len_for(1000), 10);
    }

    /// A `/query/miner/pending` reply shaped exactly like the node's.
    fn pending_reply(height: u64, prevhash: &Hash, bodies: &[&str], mkrl: &[Hash]) -> Value {
        serde_json::json!({
            "ret": 0,
            "height": height,
            "prevhash": prevhash.to_hex(),
            "block_intro": "00",
            "target_hash": "ff",
            "coinbase_body": bodies.first().copied().unwrap_or("00"),
            "transaction_body_list": bodies,
            "mkrl_modify_list": mkrl.iter().map(|h| h.to_hex()).collect::<Vec<String>>(),
        })
    }

    #[test]
    fn a_pending_reply_for_this_very_height_and_parent_is_accepted() {
        let prev = leaf(9);
        // Node coinbase + two transactions -> the pool keeps the two, drops the
        // coinbase, and takes the node's 2 sibling hashes.
        let j = pending_reply(50, &prev, &["00aa", "0bb0", "0cc0"], &[leaf(1), leaf(2)]);
        let got = parse_node_packed_txs(&j, 50, &prev).expect("a matching template is usable");
        assert_eq!(got.bodies, vec![vec![0x0b, 0xb0], vec![0x0c, 0xc0]]);
        assert_eq!(got.mrklrts, vec![leaf(1), leaf(2)]);
        assert_eq!(got.block_tx_count(), 3, "our coinbase plus the node's two txs");
        // An empty mempool is a legitimate answer, not a failure.
        let empty = pending_reply(50, &prev, &["00aa"], &[]);
        let got = parse_node_packed_txs(&empty, 50, &prev).expect("an empty mempool is fine");
        assert!(got.bodies.is_empty());
        assert_eq!(got.block_tx_count(), 1);
    }

    #[test]
    fn a_pending_reply_that_is_not_for_this_block_is_refused() {
        let prev = leaf(9);
        let other = leaf(8);
        let bodies = ["00aa", "0bb0", "0cc0"];
        let sib = [leaf(1), leaf(2)];
        // The chain moved under us: those transactions were validated for another
        // height, and mining them here risks the whole block reward.
        assert!(parse_node_packed_txs(&pending_reply(51, &prev, &bodies, &sib), 50, &prev).is_err());
        // Same height, different parent: a reorg repacked against another state.
        assert!(
            parse_node_packed_txs(&pending_reply(50, &other, &bodies, &sib), 50, &prev).is_err()
        );
        // The node has the miner switched off, so it will not pack at all.
        let off = serde_json::json!({"ret":1,"err":"miner not enabled"});
        let err = parse_node_packed_txs(&off, 50, &prev).expect_err("refused");
        assert!(err.contains("miner not enabled"), "{err}");
        // The node was unreachable: `get_json` turns that into this shape.
        let down = serde_json::json!({"http_error":"connection refused"});
        assert!(parse_node_packed_txs(&down, 50, &prev).is_err());
    }

    #[test]
    fn a_packed_set_is_carried_over_only_for_the_very_same_block() {
        let packed = PackedTxs {
            bodies: vec![vec![0xaa]],
            mrklrts: vec![leaf(1)],
        };
        let current = tpl_with(packed.clone());
        // Same height, same parent: the node would serve the same set, and the
        // pool would not swap the template anyway. Reuse it.
        let same = tpl_with(PackedTxs::default());
        assert!(packed_txs_still_apply(Some(&current), &same).is_some());
        // A new height, or a same-height reorg onto another parent, is a
        // different block: those transactions were validated against a state
        // this template does not extend.
        let mut next = tpl_with(PackedTxs::default());
        next.height += 1;
        assert!(packed_txs_still_apply(Some(&current), &next).is_none());
        let mut reorg = tpl_with(PackedTxs::default());
        reorg.prevhash = leaf(4);
        assert!(packed_txs_still_apply(Some(&current), &reorg).is_none());
        // Nothing to carry over: keep asking, so a node that starts serving its
        // mempool is picked up and the reason it will not stays current.
        let empty = tpl_with(PackedTxs::default());
        assert!(packed_txs_still_apply(Some(&empty), &same).is_none());
        assert!(packed_txs_still_apply(None, &same).is_none());
    }

    #[test]
    fn a_sibling_list_that_does_not_fit_the_transaction_count_is_refused() {
        // A merkle root built from the wrong sibling list is one the node cannot
        // reproduce, so the block is thrown away with its entire reward. Refusing
        // the set costs the fees of one block; accepting it costs the block.
        let prev = leaf(9);
        let bodies = ["00aa", "0bb0", "0cc0", "0dd0"]; // 4 txs -> needs 2 siblings
        let short = pending_reply(50, &prev, &bodies, &[leaf(1)]);
        let err = parse_node_packed_txs(&short, 50, &prev).expect_err("refused");
        assert!(err.contains("merkle sibling"), "{err}");
        let long = pending_reply(50, &prev, &bodies, &[leaf(1), leaf(2), leaf(3)]);
        assert!(parse_node_packed_txs(&long, 50, &prev).is_err());
        let right = pending_reply(50, &prev, &bodies, &[leaf(1), leaf(2)]);
        assert!(parse_node_packed_txs(&right, 50, &prev).is_ok());
    }
}
