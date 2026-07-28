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

/// What the node said when it was asked for an address's balance.
///
/// Three states, not two. This used to be a bare `String` that folded every
/// failure into "", which `balance_units` then valued as a confident zero. A
/// node that was down, restarting, or answering with its own error object read
/// exactly like a wallet holding nothing: settlement published "matured = 0" and
/// flagged it CURRENT, so every miner polling `/earnings` was told it was owed
/// nothing for as long as the outage lasted, and the template loop re-poisoned
/// the same figure every 30 seconds. Miners whose shares rolled out of the PPLNS
/// window during the outage were never paid for that work.
///
/// A wallet holding nothing is NOT this state: the node always emits the
/// `hacash` field and renders an empty wallet as "0:0", which is
/// [`Reported`](BalanceAnswer::Reported) and values as a real zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BalanceAnswer {
    /// The node gave a balance string for the address, e.g. "1:248" or "0:0".
    Reported(String),
    /// The node answered, but not with a balance: its own `{"ret":1,...}` error
    /// object, or a body carrying no `hacash` field at all.
    Refused(String),
    /// Nothing usable came back: connection refused, a timeout, a proxy's error
    /// page. The wallet is UNKNOWN, not empty.
    NoAnswer(String),
}

impl BalanceAnswer {
    /// The balance in whole units of 0.1 HAC, or `None` when the pool must not
    /// act on this answer at all. Anything but a reported balance is `None`:
    /// callers already treat `None` as "skip this cycle, keep the last good
    /// figure and mark it stale", which is exactly right for a silent node.
    pub fn units(&self) -> Option<u64> {
        match self {
            BalanceAnswer::Reported(s) => balance_units(s),
            BalanceAnswer::Refused(_) | BalanceAnswer::NoAnswer(_) => None,
        }
    }
}

impl std::fmt::Display for BalanceAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BalanceAnswer::Reported(s) => write!(f, "{s}"),
            BalanceAnswer::Refused(s) => write!(f, "the node refused to report a balance: {s}"),
            BalanceAnswer::NoAnswer(s) => write!(f, "no answer from the node: {s}"),
        }
    }
}

/// How much of an unusable answer goes into a log line. A non-JSON body can be a
/// whole HTML error page, and a log that scrolls the real message away is a log
/// nobody can read during the outage it is describing.
const ANSWER_EXCERPT_CHARS: usize = 200;

/// Truncate on a CHARACTER boundary: this text comes off the wire and slicing it
/// by bytes would panic the caller on any multi-byte error message.
fn excerpt(s: &str) -> String {
    s.chars().take(ANSWER_EXCERPT_CHARS).collect()
}

/// Classify a `/query/balance` response. Fails SAFE: only an answer that really
/// carries a balance becomes [`BalanceAnswer::Reported`], and everything else is
/// a state the caller must refuse to pay on.
pub fn balance_answer(j: &Value) -> BalanceAnswer {
    // get_json encodes a transport failure as {"http_error": "..."} and a
    // non-JSON body as a bare string. Neither is the node speaking.
    if let Some(e) = j.get("http_error").and_then(|v| v.as_str()) {
        return BalanceAnswer::NoAnswer(excerpt(e));
    }
    if !j.is_object() {
        return BalanceAnswer::NoAnswer(excerpt(&j.to_string()));
    }
    // The node answered, and its answer is "no": a bad address, too many
    // addresses, an unreadable state. There is no balance in it to pay on.
    if find_u64(j, "ret").is_some_and(|r| r != 0) {
        return BalanceAnswer::Refused(excerpt(&j.to_string()));
    }
    match find_str(j, "hacash") {
        Some(s) if !s.trim().is_empty() => BalanceAnswer::Reported(s),
        // ret=0 with no `hacash` is a shape this pool does not recognise. The
        // node always emits the field, so its absence means we are not talking
        // to one - never that the wallet is empty.
        _ => BalanceAnswer::Refused(excerpt(&j.to_string())),
    }
}

/// The address's "hacash" balance as the node reported it, or why it did not.
pub fn balance(client: &reqwest::blocking::Client, base: &str, addr: &str) -> BalanceAnswer {
    balance_answer(&get_json(
        client,
        &format!("{base}/query/balance?address={addr}"),
    ))
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
/// of the whole u64 range off one malformed response.
///
/// An EMPTY string is one of those refusals, and it is the important one. The
/// node always emits the `hacash` field and renders a wallet holding nothing as
/// "0:0", so "" is never something it reported: it is what the old reader
/// produced when there was no answer at all. Valuing it as `Some(0)` told the
/// settlement that a wallet it could not see was empty, and told every miner
/// polling `/earnings` that it was owed nothing for the length of the outage.
pub fn balance_units(bal: &str) -> Option<u64> {
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

/// The coinbase subsidy of the block at `height`, in units of 0.1 HAC
/// (`block_reward` is a whole number of HAC = unit 248).
///
/// This is NOT the whole income a found block brings in. The pool packs the
/// node's transactions, and the chain credits the sum of their fees to the
/// coinbase address as well - the same wallet the pool settles from. See
/// [`block_fees`] for that half of it.
pub fn block_reward_units(height: u64) -> u64 {
    mint::genesis::block_reward_number(height) as u64 * 10
}

/// Fine steps in one payout unit of 0.1 HAC; one step is 10^-9 HAC.
///
/// A transaction fee is routinely a thousandth of a payout unit, so a block's
/// fees are summed on this finer scale and rounded up to whole units only once,
/// at the end. Rounding each fee up on its own would hold back a whole unit per
/// transaction and freeze real money for a whole maturity window.
const FEE_FINE_PER_UNIT: u128 = 100_000_000;

/// The largest fee total this pool will believe, on the fine scale. Past the
/// whole coin supply the answer is corrupt or hostile, not a rich block, and
/// turning it into a hold-back would stop every payout the pool ever makes.
const MAX_FEE_FINE: u128 = MAX_PLAUSIBLE_UNITS as u128 * FEE_FINE_PER_UNIT;

/// A node "mantissa:unit" amount on the fine scale, ROUNDED UP.
///
/// Rounds UP because this number becomes money the pool refuses to pay out yet.
/// Rounding a fee down to nothing is exactly how the fee ends up distributed at
/// zero confirmations, which is the failure this exists to stop.
///
/// `None` is "this is not an amount I can value" - a negative mantissa, a
/// missing separator, an exponent no wallet could hold - and the caller must
/// then refuse to settle rather than read it as a zero fee.
pub fn fin_fine_ceil(amount: &str) -> Option<u128> {
    let (m, u) = amount.split_once(':')?;
    let (Ok(m), Ok(u)) = (m.trim().parse::<u128>(), u.trim().parse::<i64>()) else {
        return None;
    };
    if !(0..=255).contains(&u) {
        return None; // the chain's unit is a u8; anything else is not its answer
    }
    if m == 0 {
        return Some(0); // a real zero, at any unit
    }
    // value = m * 10^(u-248) HAC, and one fine step is 10^-9 HAC.
    let exp = u - 239;
    let fine = if exp >= 0 {
        let scale = u32::try_from(exp)
            .ok()
            .and_then(|e| 10u128.checked_pow(e))?;
        m.checked_mul(scale)?
    } else {
        match u32::try_from(-exp).ok().and_then(|e| 10u128.checked_pow(e)) {
            Some(d) => m.div_ceil(d),
            // Finer than a fine step by more orders of magnitude than a u128 can
            // express. It is still money, so it still counts as one step.
            None => 1,
        }
    };
    (fine <= MAX_FEE_FINE).then_some(fine)
}

/// Fine steps as whole payout units of 0.1 HAC, rounded UP.
///
/// The wallet balance the pool settles against is itself floored to whole units,
/// and a fee that straddles a unit boundary can push that floor up by one. The
/// ceiling is what makes the hold-back cover that case instead of leaving one
/// unit payable out of income a reorg can still revoke.
pub fn fine_to_units_ceil(fine: u128) -> u64 {
    fine.div_ceil(FEE_FINE_PER_UNIT)
        .min(MAX_PLAUSIBLE_UNITS as u128) as u64
}

/// What the node says about the transaction fees one of OUR blocks credited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockFees {
    /// The chain holds our block at that height, and it credited this much fee
    /// income to the pool wallet, in units of 0.1 HAC rounded up.
    Counted(u64),
    /// The node answered, and the chain does NOT hold our block at that height.
    /// It credited nothing there - no subsidy and no fee - so there is no fee
    /// income to hold back.
    NotOnChain,
    /// No usable answer. This is NOT a zero fee: the wallet may be holding fee
    /// income the pool cannot value, so the caller must refuse to settle.
    Unknown(String),
}

/// Which transactions the chain says are in our block, or why it cannot say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockTxs {
    /// The chain holds OUR block at that height, and these are the hashes of the
    /// transactions in it. The coinbase is not among them: it pays no fee.
    Ours(Vec<String>),
    /// The node answered, and the chain does not hold our block there.
    NotOnChain,
    /// No usable answer.
    Unknown(String),
}

/// Read a `/query/block/intro?tx_hash_list=true` answer for one of OUR blocks.
///
/// Split out from [`block_fees`] so the decision - price it, ignore it, or stop
/// settling - is testable without a node. Fails SAFE: only an answer that really
/// carries our block's transaction list is [`BlockTxs::Ours`].
pub fn block_txs_of(j: &Value, our_hash_hex: &str) -> BlockTxs {
    // get_json encodes a transport failure as {"http_error": "..."} and a
    // non-JSON body as a bare string. Neither is the node speaking.
    if !j.is_object() || j.get("http_error").is_some() {
        return BlockTxs::Unknown(excerpt(&j.to_string()));
    }
    let Some(ret) = find_u64(j, "ret") else {
        return BlockTxs::Unknown(excerpt(&j.to_string()));
    };
    if ret != 0 {
        // The node is up and has no block at that height: ours was refused, or
        // has not been inserted yet. Either way it has credited nothing.
        return BlockTxs::NotOnChain;
    }
    let Some(hash) = find_str(j, "hash") else {
        return BlockTxs::Unknown(excerpt(&j.to_string()));
    };
    if !hash.eq_ignore_ascii_case(our_hash_hex) {
        return BlockTxs::NotOnChain; // another block won that height
    }
    // ret=0 for our block but no list at all is an answer this pool does not
    // recognise - never "the block had no transactions". A node that quietly
    // dropped the field would otherwise read as a zero fee on every block.
    let Some(list) = find_value(j, "tx_hash_list").and_then(|v| v.as_array()) else {
        return BlockTxs::Unknown(excerpt(&j.to_string()));
    };
    match list
        .iter()
        .map(|h| h.as_str().map(|s| s.to_string()))
        .collect::<Option<Vec<String>>>()
    {
        Some(hs) => BlockTxs::Ours(hs),
        None => BlockTxs::Unknown(excerpt(&j.to_string())),
    }
}

/// The `fee_got` in an answer to `/query/transaction`, on the fine scale.
///
/// `fee_got` and not `fee`: what the chain adds to the coinbase address is the
/// fee the transaction actually PAID for its place in the block, which a
/// fee-raise or a gas refund can make smaller than the fee it declared.
pub fn fee_got_fine(j: &Value) -> Option<u128> {
    if !j.is_object() || j.get("http_error").is_some() {
        return None;
    }
    if find_u64(j, "ret") != Some(0) {
        return None;
    }
    fin_fine_ceil(&find_str(j, "fee_got")?)
}

/// What the chain credited the pool wallet in TRANSACTION FEES for our block at
/// `height`, in units of 0.1 HAC rounded up.
///
/// The figure cannot be taken from the block the pool built: its transaction
/// bodies are raw bytes the pool deliberately has no codec for, and
/// `/submit/block` answers only `{"ok":true}`. So it is read back off the node
/// once the block exists, one `/query/transaction` per packed transaction.
///
/// Answers `Unknown` on anything short of a definitive reply, because the caller
/// turns that into "settle nothing this cycle". The alternative - treating an
/// unreachable node as a zero fee - pays that fee income out at zero
/// confirmations, and an orphan then leaves the operator funding a payout out of
/// a block the chain no longer has.
pub fn block_fees(
    client: &reqwest::blocking::Client,
    node: &str,
    height: u64,
    our_hash_hex: &str,
) -> BlockFees {
    let j = get_json(
        client,
        &format!("{node}/query/block/intro?height={height}&tx_hash_list=true"),
    );
    let hashes = match block_txs_of(&j, our_hash_hex) {
        BlockTxs::Ours(hs) => hs,
        BlockTxs::NotOnChain => return BlockFees::NotOnChain,
        BlockTxs::Unknown(why) => return BlockFees::Unknown(why),
    };
    let mut fine: u128 = 0;
    for h in &hashes {
        let t = get_json(client, &format!("{node}/query/transaction?hash={h}"));
        let Some(f) = fee_got_fine(&t) else {
            return BlockFees::Unknown(format!("transaction {h}: {}", excerpt(&t.to_string())));
        };
        fine = fine.saturating_add(f);
        if fine > MAX_FEE_FINE {
            return BlockFees::Unknown(format!("fees at height {height} exceed any real block"));
        }
    }
    BlockFees::Counted(fine_to_units_ceil(fine))
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
pub fn verify_admitted(client: &reqwest::blocking::Client, node: &str, txhash: &str) -> Admission {
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
/// `immature_units` is the WHOLE income of blocks the pool found that are not
/// yet buried deep enough to be final. Whole, because the chain credits the
/// coinbase address both the subsidy and the sum of the fees of every
/// transaction in the block, and this pool packs the node's transactions: a
/// hold-back of the subsidy alone leaves the fees payable here. Distributing
/// either and then losing the block to a reorg is an unrecoverable operator
/// loss: the income disappears from the canonical chain while the payout
/// transaction that spent it stays valid.
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
/// fsyncs the bytes before the rename and the directory after it, and FAILS if
/// either fsync fails.
pub fn atomic_write(path: &str, body: &[u8], durable: bool) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = format!("{path}.tmp.{}", std::process::id());
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body)?;
        if durable {
            // This used to be `let _ = f.sync_all()`. `durable` is the promise
            // the settlement path broadcasts a payout on, and a discarded error
            // here answers "recorded" for bytes that only ever reached the page
            // cache - a full disk, an I/O error, or a network mount that went
            // away all report themselves at flush time and nowhere else. Lose
            // power in the seconds that follow and the pool restarts with no
            // memory of the transaction it signed, so the next cycle signs a
            // SECOND payout for the same PPLNS window and the operator funds
            // the difference out of their own wallet.
            f.sync_all()?;
        }
    }
    std::fs::rename(&tmp, path)?;
    if durable {
        // The bytes can be on the platter while the directory entry pointing at
        // them is not: the rename is its own metadata change and is lost on its
        // own. That leaves the PREVIOUS state file in place - the one without
        // the payout hash or without the immature hold-back - which costs
        // exactly what the paragraph above costs.
        fsync_parent_dir(path)?;
    }
    Ok(())
}

/// fsync the directory that holds `path`, so a rename into it survives a power
/// cut rather than only the bytes it points at.
///
/// A bare filename (a relative `wallet_file` in the config) has no directory
/// component and must fall back to the working directory: treating that as a
/// failure would make every durable write fail and stop the pool paying anyone.
#[cfg(unix)]
fn fsync_parent_dir(path: &str) -> std::io::Result<()> {
    let dir = match std::path::Path::new(path).parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    std::fs::File::open(dir)?.sync_all()
}

/// Windows has no directory fsync: a directory handle cannot be opened for
/// `FlushFileBuffers`, so there is nothing to call and the durability of the
/// rename is NTFS's own metadata journal. Reporting that as a failure would
/// refuse every settlement the pool ever tried, which is worse than the gap it
/// would be reporting. The data fsync above still holds on this platform.
#[cfg(not(unix))]
fn fsync_parent_dir(_path: &str) -> std::io::Result<()> {
    Ok(())
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

/// How long a share may go on earning credit, and how long the credit an evicted
/// share already earned survives, expressed in settlement intervals.
///
/// One interval is the unit that matters because that is the longest a miner can
/// usefully sit on shares: the pool pins a template for a block interval, and it
/// publishes the settlement interval in `/terms`. Anything shorter would let a
/// hoarder time its dump; much longer and the split stops tracking who is mining
/// now.
const PPLNS_HORIZON_INTERVALS: u64 = 1;

/// The documented default settlement interval. `hbit-pool-server`'s `usage()`
/// quotes it and `hbit-pool-payout` falls back to it when it has to read an
/// accounting file that does not record the interval the server was running.
pub const DEFAULT_SETTLE_SECS: u64 = 300;

/// The credit horizon in milliseconds for a pool settling every `settle_secs`.
pub fn pplns_horizon_ms(settle_secs: u64) -> u64 {
    settle_secs
        .saturating_mul(PPLNS_HORIZON_INTERVALS)
        .saturating_mul(1_000)
        .max(1_000)
}

/// Read the persisted share window, accepting BOTH the timestamped form this
/// pool writes now and the bare list of worker ids older builds wrote.
///
/// An older file carries no arrival times at all. Every share in it is given the
/// SAME stamp, `fallback_ms`, because that is the only assumption that treats
/// every miner alike: credit is proportional, so one common start time preserves
/// the split exactly, while inventing different ages would silently move money
/// between miners on a restart.
pub fn parse_share_order(j: &Value, fallback_ms: u64) -> Vec<(String, u64)> {
    j.get("order")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    if let Some(s) = x.as_str() {
                        return Some((s.to_string(), fallback_ms));
                    }
                    let row = x.as_array()?;
                    let w = row.first()?.as_str()?.to_string();
                    let at = row.get(1)?.as_u64().unwrap_or(fallback_ms);
                    Some((w, at))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Read the banked credit of shares that have already left the window. Absent in
/// a file written before shares were timed, which reads as "none banked" rather
/// than as a corrupt file.
pub fn parse_banked_credit(j: &Value) -> Vec<(u64, Vec<(String, u64)>)> {
    j.get("banked")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    let at = x.get("at").and_then(|v| v.as_u64())?;
                    let rows = x
                        .get("rows")
                        .and_then(|v| v.as_array())
                        .map(|r| {
                            r.iter()
                                .filter_map(|e| {
                                    let row = e.as_array()?;
                                    Some((
                                        row.first()?.as_str()?.to_string(),
                                        row.get(1)?.as_u64()?,
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Some((at, rows))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Rebuild the PPLNS payout credit from the pool's own accounting file.
///
/// The manual payout tool needs this because the server holds the wallet's
/// settlement lock for its whole run: if the tool is able to settle at all then
/// the server is stopped, so its `/stats` endpoint cannot answer and the file it
/// left behind is the authority on who is owed what.
///
/// It returns CREDIT, not share counts, for the same reason the server settles on
/// credit: a headcount taken at the instant of a payout is a number one miner can
/// own outright by dumping a window's worth of withheld shares, and this tool
/// signs the same money.
pub fn load_pplns_credit(state_file: &str) -> Vec<(String, u64)> {
    let Some(j) = read_state_json(state_file) else {
        return Vec::new();
    };
    let window = j
        .get("window")
        .and_then(|v| v.as_u64())
        .unwrap_or(PPLNS_WINDOW as u64) as usize;
    // The horizon the SERVER was running, so the manual tool splits money the
    // same way the automatic settlement would have.
    let horizon = j
        .get("credit_horizon_ms")
        .and_then(|v| v.as_u64())
        .filter(|h| *h > 0)
        .unwrap_or_else(|| pplns_horizon_ms(DEFAULT_SETTLE_SECS));
    let at = credit_anchor_ms(&j, pool_core::now_ms());
    // A file with no arrival times is read as if every share landed one horizon
    // before that instant: they are all treated alike, so the split is the one
    // the old build would have made, and no share reads as newer than it is.
    let order = parse_share_order(&j, at.saturating_sub(horizon));
    let banked = parse_banked_credit(&j);
    if order.is_empty() && banked.is_empty() {
        return Vec::new();
    }
    pool_core::Pplns::restore(window, horizon, order, banked).credit(at)
}

/// The instant a stored share window is worth valuing at: the last moment the
/// pool that wrote the file was actually accounting.
///
/// NOT the wall clock. Credit is residence in the window, and nothing enters or
/// leaves that window while the server is stopped - which it always is when this
/// tool runs, because the tool can only get the settlement lock if it is. Valuing
/// at the wall clock let the whole window age together while nothing happened,
/// and past one horizon that undoes the fix this file exists to carry: every
/// share caps at the horizon, so the split flattens back to a HEADCOUNT, and the
/// banked credit of the miners a dump evicted expires entirely. A miner that
/// withheld a window's worth and dumped it before the server stopped would then
/// take the lot - the exact attack, back again, on the settler an operator
/// reaches for when the server is down. Five minutes between stopping the pool
/// and running the payout is all it took.
///
/// Anchoring here also makes the payout DETERMINISTIC: the same file settles the
/// same way whether the operator runs the tool immediately or an hour later.
///
/// The tool's own clock is deliberately not consulted when the file has times of
/// its own. Every credit figure is a DIFFERENCE against this instant, so a
/// consistent anchor taken from the file makes the split independent of what the
/// machine running the settlement thinks the time is.
///
/// Falls back to `now_ms` when the file carries no times at all (a window written
/// before shares were stamped), because there is nothing to anchor to and the
/// fallback stamp one horizon back then weighs every share alike, as it must.
pub fn credit_anchor_ms(j: &Value, now_ms: u64) -> u64 {
    let newest_share = j
        .get("order")
        .and_then(|v| v.as_array())
        .and_then(|a| a.iter().filter_map(|x| x.as_array()?.get(1)?.as_u64()).max());
    let newest_bank = j
        .get("banked")
        .and_then(|v| v.as_array())
        .and_then(|a| a.iter().filter_map(|x| x.get("at")?.as_u64()).max());
    newest_share
        .into_iter()
        .chain(newest_bank)
        .max()
        .unwrap_or(now_ms)
}

/// One block of not-yet-final income the pool server is holding back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImmatureBlock {
    pub height: u64,
    /// OUR block's hash at that height, hex. The income is only real while the
    /// chain still holds this hash there.
    pub hash: String,
    /// What it put into the pool wallet so far, in units of 0.1 HAC.
    pub units: u64,
    /// Are that block's TRANSACTION FEES already inside `units`?
    ///
    /// False means `units` is the coinbase subsidy alone, and the block's fees -
    /// which the chain credits to the very same wallet - are still sitting in
    /// the balance unaccounted for. Paying against that balance hands those fees
    /// out at zero confirmations.
    pub fees_counted: bool,
}

/// Every block of not-yet-final income the pool server recorded. The manual
/// payout tool reads it so it applies the SAME maturity gate as the automatic
/// settlement instead of paying at the tip.
///
/// `fees_counted` defaults to FALSE when the field is absent, because a file
/// written by a build that held back only the subsidy really does carry the
/// subsidy alone. Defaulting the other way would silently distribute those
/// blocks' fees on the first settlement after an upgrade.
pub fn load_immature_blocks(state_file: &str) -> Vec<ImmatureBlock> {
    let Some(j) = read_state_json(state_file) else {
        return Vec::new();
    };
    j.get("immature")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some(ImmatureBlock {
                        height: x.get("height").and_then(|v| v.as_u64())?,
                        hash: x
                            .get("hash")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        units: x.get("units").and_then(|v| v.as_u64())?,
                        fees_counted: x
                            .get("fees_counted")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
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
    /// The exact signed bytes that were submitted, hex-encoded.
    ///
    /// Kept because a node that once HELD this transaction also relayed it, and
    /// the mempool is memory-only: a routine node restart empties it, and the
    /// pool then asks about a hash the node no longer knows. Re-splitting and
    /// re-signing that window makes a DIFFERENT transaction (fresh timestamp,
    /// so a different hash); replay protection on this chain is by hash alone,
    /// so both can be mined and the operator pays the same miners twice out of
    /// its own wallet. With the bytes here the pool re-broadcasts the identical
    /// transaction, which can only ever be included once.
    pub body_hex: String,
    /// (worker address, units of 0.1 HAC) exactly as the transaction pays them.
    pub rows: Vec<(String, u64)>,
}

impl PayoutRecord {
    /// Total this transaction pays, in units of 0.1 HAC.
    pub fn units(&self) -> u64 {
        self.rows
            .iter()
            .map(|(_, u)| *u)
            .fold(0u64, |a, b| a.saturating_add(b))
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
            "body_hex": self.body_hex,
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
            // A record written before the bytes were kept reads as "no bytes",
            // which `gone_action` treats as un-rebroadcastable rather than as
            // safe to re-issue. Missing evidence must never become permission
            // to sign the same window again.
            body_hex: v
                .get("body_hex")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
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

/// What to do about a payout the node answers "I do not know that hash" for.
///
/// [`PayoutTxState::Gone`] is NOT the same fact as "nothing was paid". The node's
/// mempool lives in memory, so the very same answer comes back for a transaction
/// the node validated, accepted, inserted and RELAYED, and then lost to a restart
/// or an eviction. That transaction is still signed, still valid, and any peer
/// holding it can still mine it.
///
/// Treating that as "nothing was paid" is what costs real money: the pool
/// re-splits the same window and signs a second transaction with a fresh
/// timestamp, so a different hash, and this chain's replay protection is by hash
/// alone. Both confirm, and the operator has paid the same miners twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoneAction {
    /// Nothing to put back and nothing that could have been relayed: a tracked
    /// hash with no record behind it, or a record from a build that kept neither
    /// the bytes nor any sighting of the node holding it. Forget the hash; its
    /// rows (if there are any) are genuinely still owed.
    Forget,
    /// The bytes are still on hand and this transaction's fate is unknown - the
    /// node held it once, or the pool never got a usable answer about it.
    /// Re-broadcast the IDENTICAL signed bytes (same hash, so it can only be
    /// mined once) and keep tracking it. Never re-sign the window.
    Rebroadcast,
    /// The node held it, but there are no stored bytes to re-broadcast (a record
    /// from a build that did not keep them). Keep tracking it and say so: a
    /// stalled payout the operator can see beats a duplicate payout nobody can
    /// take back.
    Stuck,
}

/// Decide [`GoneAction`] from what the pool recorded about the payout.
///
/// The test is whether there are BYTES, not whether `node_holds` was ever set.
///
/// `node_holds` is set only when [`verify_admitted`] came back `Held`, and the
/// two answers that leave it false are exactly the two where relay cannot be
/// ruled out: a submit that timed out ([`SubmitVerdict::Unresolved`]) and a
/// verification that could not be read ([`Admission::Unresolved`]). In both the
/// bytes went onto the wire, and the node may have validated, inserted and
/// relayed them before the answer was lost. Keying on `node_holds` therefore
/// read "I have no proof it was relayed" as "it was never relayed": the rows went
/// back on the owed ledger, the next cycle re-split them into a transaction with
/// a fresh timestamp and so a different hash, and with replay protection by hash
/// alone BOTH could be mined. That is the double payout this whole enum exists to
/// prevent, arriving through the pool's own submit path.
///
/// A record only ever exists because the pool was about to post those exact
/// bytes: it is written durably immediately before the post, and the two verdicts
/// that definitively rule relay out - the node's own validator refusing it, and
/// `Admission::Missing` - drop the record inline and never reach here. So a
/// record that still has its bytes is one whose fate is unknown, and the only
/// safe move is to put the SAME transaction back on the wire. It can be mined
/// once however many peers hold it, and if it never lands the payout stalls where
/// an operator can see it rather than being paid twice where nobody can take it
/// back. Nothing on this chain expires a signed transaction, so a re-broadcast
/// the node accepts always resolves.
///
/// `None` (a tracked hash with no record behind it) has no bytes to re-broadcast
/// and no rows to owe anyone, so keeping it forever would freeze every later
/// payout with nothing to show for it.
pub fn gone_action(rec: Option<&PayoutRecord>) -> GoneAction {
    match rec {
        Some(r) if !r.body_hex.is_empty() => GoneAction::Rebroadcast,
        Some(r) if r.node_holds => GoneAction::Stuck,
        _ => GoneAction::Forget,
    }
}

/// What `/submit/transaction` said about a payout we just posted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitVerdict {
    /// `ret=0`: the API took the bytes. Still not proof the node holds it - only
    /// [`verify_admitted`] is that.
    Accepted,
    /// The node itself answered with a non-zero `ret`: it refused the
    /// transaction during synchronous validation, so it never inserted it into
    /// the mempool and never relayed it.
    Rejected,
    /// No verdict at all: [`post_hex`] returns the plain string
    /// `"http_error: ..."` when the request times out or the connection drops,
    /// and a timeout happens AFTER the node may have already taken and relayed
    /// the transaction. Reading that as a rejection and forgetting the hash is
    /// how a payout gets issued a second time.
    Unresolved,
}

/// Classify a `/submit/transaction` response body.
///
/// Fails SAFE: anything that is not the node speaking a `ret` we can read is
/// `Unresolved`, which keeps the payout tracked for the next cycle's poll.
pub fn submit_verdict(resp: &str) -> SubmitVerdict {
    let Ok(j) = serde_json::from_str::<Value>(resp) else {
        return SubmitVerdict::Unresolved; // "http_error: ..." lands here
    };
    if j.get("http_error").is_some() {
        return SubmitVerdict::Unresolved;
    }
    match find_u64(&j, "ret") {
        Some(0) => SubmitVerdict::Accepted,
        Some(_) => SubmitVerdict::Rejected,
        None => SubmitVerdict::Unresolved,
    }
}

/* ---------------------------------------------------------------------------
 * The owed ledger.
 *
 * When a settlement chunk definitively does not happen, the money it carried
 * does not simply return to the pot: it is owed to the exact miners that chunk
 * named. Dropping the rows and letting the next cycle re-split the whole balance
 * over the live PPLNS window hands that money to whoever is mining now -
 * including the miners whose chunks DID go through, who are then paid twice for
 * the same window while the miners in the failed chunk are paid once, or never.
 *
 * These rows are therefore persisted and paid FIRST, before a single unit of
 * fresh income is split.
 * ------------------------------------------------------------------------- */

/// Add the rows of a payout that definitively did not happen to the owed ledger.
pub fn owe_rows(owed: &mut Vec<(String, u64)>, rows: &[(String, u64)]) {
    for (w, u) in rows {
        if *u == 0 {
            continue;
        }
        match owed.iter_mut().find(|(x, _)| x == w) {
            Some(e) => e.1 = e.1.saturating_add(*u),
            None => owed.push((w.clone(), *u)),
        }
    }
}

/// Take off the owed ledger what a payout the pool has now RECORDED carries.
///
/// Saturating, and only ever downward: a row paying more than is owed (an owed
/// row and a fresh share to the same miner, merged into one action) clears the
/// debt and no more.
pub fn deduct_owed(owed: &mut Vec<(String, u64)>, rows: &[(String, u64)]) {
    for (w, u) in rows {
        if let Some(e) = owed.iter_mut().find(|(x, _)| x == w) {
            e.1 = e.1.saturating_sub(*u);
        }
    }
    owed.retain(|(_, u)| *u > 0);
}

/// The rows this cycle must pay BEFORE it splits any fresh income, and what is
/// left to split after them.
///
/// Owed rows are taken in order and partially where the balance runs out, so one
/// large debt cannot starve while smaller ones keep being paid around it. What is
/// not taken stays on the ledger for the next cycle.
pub fn take_owed(owed: &[(String, u64)], distributable: u64) -> (Vec<(String, u64)>, u64) {
    let mut left = distributable;
    let mut rows: Vec<(String, u64)> = Vec::new();
    for (w, u) in owed {
        if left == 0 {
            break;
        }
        let pay = (*u).min(left);
        if pay == 0 {
            continue;
        }
        left -= pay;
        rows.push((w.clone(), pay));
    }
    (rows, left)
}

/// Fold rows paying the same address into one action, keeping first-seen order.
///
/// An owed row and a fresh share for the same miner would otherwise be two
/// actions in one transaction, and each action costs against the node's
/// TX_ACTIONS_MAX limit that `PAYOUT_CHUNK` is sized against.
pub fn merge_payout_rows(rows: &mut Vec<(String, u64)>) {
    let mut at: HashMap<String, usize> = HashMap::with_capacity(rows.len());
    let mut out: Vec<(String, u64)> = Vec::with_capacity(rows.len());
    for (w, u) in rows.drain(..) {
        match at.get(&w) {
            Some(i) => out[*i].1 = out[*i].1.saturating_add(u),
            None => {
                at.insert(w.clone(), out.len());
                out.push((w, u));
            }
        }
    }
    *rows = out;
}

/// The owed ledger as it is stored in the pool state file.
pub fn owed_to_json(owed: &[(String, u64)]) -> Value {
    Value::Array(
        owed.iter()
            .map(|(w, u)| serde_json::json!([w, u]))
            .collect(),
    )
}

/// Read the owed ledger out of an already-parsed state document. A row the file
/// cannot describe is dropped rather than guessed at: an unreadable amount must
/// never become a payment.
pub fn parse_owed(j: &Value) -> Vec<(String, u64)> {
    j.get("owed")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    let x = r.as_array()?;
                    let w = x.first()?.as_str()?.to_string();
                    let u = x.get(1)?.as_u64()?;
                    (!w.is_empty() && u > 0).then_some((w, u))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The owed ledger. Losing it means the miners in a failed chunk are never paid
/// for that window, so it is persisted with everything else.
pub fn load_owed(state_file: &str) -> Vec<(String, u64)> {
    let Some(j) = read_state_json(state_file) else {
        return Vec::new();
    };
    parse_owed(&j)
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

/// Replace the WHOLE settlement ledger (pending hashes, per-transaction rows,
/// what is still owed, and confirmed totals) in the pool state file, preserving
/// every other field.
///
/// One write, because the four move together: a payout leaves the in-flight rows
/// at the same instant it enters the paid totals, and a chunk that failed leaves
/// them at the same instant its rows become owed. A crash between any two would
/// either lose a payment or count it twice.
pub fn save_settlement_ledger(
    state_file: &str,
    hashes: &[String],
    records: &[PayoutRecord],
    owed: &[(String, u64)],
    paid: &PaidLedger,
) -> std::io::Result<()> {
    let mut j = read_state_json(state_file).unwrap_or_else(|| serde_json::json!({}));
    j["settle_pending_txs"] = serde_json::json!(hashes);
    j["payouts_inflight"] = Value::Array(records.iter().map(|r| r.to_json()).collect());
    j["owed"] = owed_to_json(owed);
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
    let v = j
        .get(key)
        .and_then(|v| v.as_u64())
        .unwrap_or(default as u64);
    if v == 0 || v > max as u64 {
        return Err(format!(
            "its `{key}` is outside the range this build accepts"
        ));
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
        envelope_u32(
            &j,
            "kdf_m_cost_kb",
            WALLET_KDF_M_COST_KB,
            WALLET_KDF_MAX_M_COST_KB,
        )
        .map_err(EnvelopeError::Shape)?,
        envelope_u32(&j, "kdf_t_cost", WALLET_KDF_T_COST, WALLET_KDF_MAX_T_COST)
            .map_err(EnvelopeError::Shape)?,
        envelope_u32(&j, "kdf_p_cost", WALLET_KDF_P_COST, WALLET_KDF_MAX_P_COST)
            .map_err(EnvelopeError::Shape)?,
    )
    .map_err(|e| {
        shape(format!(
            "its key-derivation settings cannot be used here ({e})"
        ))
    })?;
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
            eprintln!(
                "[wallet] WARNING: the encrypted form of {path} did not verify; leaving it as-is."
            );
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
        eprintln!(
            "[wallet] WARNING: could not verify the ACL of {path} ({why}); check it manually."
        );
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

/// The header timestamp a pool has ALREADY handed out for a height, so a restart
/// can reproduce the same header bytes instead of inventing new ones.
///
/// The stamp is `max(now, prev_ts + 1)` at the moment the template is first
/// fetched, and it lives in the 89-byte header every worker hashes. A pool pins
/// one template per height, so a restart part-way through a height would
/// otherwise serve a DIFFERENT header for the SAME height: measured on a rig, a
/// restart at height 350 served a stamp 68 seconds later than the one already in
/// flight. `/query/miner/notice` signals only a HEIGHT change, so nothing tells a
/// worker to reload - it goes on hashing the dead header until its current scan
/// pass ends, and every share it finds in the meantime is thrown away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StampPin {
    pub height: u64,
    pub prevhash: Hash,
    pub timestamp: u64,
}

/// The header timestamp to serve for `height` on `prevhash`.
///
/// `pin` is honoured ONLY when it describes this exact block and would produce a
/// header the node still accepts. Both guards cost a whole block reward when they
/// are wrong: `chain::verify::block_verify` rejects a block whose timestamp is
/// `<= prev_blk_time` or `> curtimes()`, and a rejected block is reported
/// asynchronously, so the pool would mine a full round into nothing and only
/// notice because the tip never reached its height.
///
/// The upper guard is `fresh`, not `now`: a pin may never move the stamp FORWARD
/// past what a fresh fetch would produce, so a stale or tampered state file
/// cannot talk the pool into mining a future-stamped block. Moving it BACKWARD to
/// a stamp this pool already served is exactly what a pool that never restarted
/// would be serving.
pub fn template_timestamp(
    pin: Option<&StampPin>,
    height: u64,
    prevhash: &Hash,
    prev_ts: u64,
    now: u64,
) -> u64 {
    let fresh = std::cmp::max(now, prev_ts.saturating_add(1));
    let Some(pin) = pin else {
        return fresh;
    };
    // A pin from another block says nothing about this one. Height alone is not
    // enough: after a same-height reorg the parent differs, and the old stamp was
    // computed against a parent whose timestamp this block no longer follows.
    if pin.height != height || pin.prevhash != *prevhash {
        return fresh;
    }
    if pin.timestamp <= prev_ts || pin.timestamp > fresh {
        return fresh;
    }
    pin.timestamp
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
    fetch_template_pinned(client, base, coinbase_addr, params, None)
}

/// `fetch_template`, reusing an already-served header timestamp when `pin`
/// describes the block being built. See `template_timestamp`.
pub fn fetch_template_pinned(
    client: &reqwest::blocking::Client,
    base: &str,
    coinbase_addr: &str,
    params: &ChainParams,
    pin: Option<&StampPin>,
) -> Option<Template> {
    let coinbase = Address::from_readable(coinbase_addr).ok()?;
    let latest = get_json(client, &format!("{base}/query/latest"));
    let prev_hei = find_u64(&latest, "height")?;
    let height = prev_hei + 1;
    let (prevhash, prev_ts, prev_diff) = if prev_hei == 0 {
        (mint::genesis::genesis_block_hash(), 1549250700u64, 0u32)
    } else {
        let ij = get_json(
            client,
            &format!("{base}/query/block/intro?height={prev_hei}"),
        );
        let ph = find_str(&ij, "hash")?;
        (
            Hash::from_hex(ph.as_bytes()).ok()?,
            find_u64(&ij, "timestamp")?,
            find_u64(&ij, "difficulty")? as u32,
        )
    };
    // The stamp is chosen BEFORE the difficulty below, because the difficulty is
    // computed from it. Reusing a pinned stamp and then recomputing the target
    // from a fresh one would put two disagreeing numbers in the same header.
    let timestamp = template_timestamp(pin, height, &prevhash, prev_ts, curtimes());
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
pub fn parse_node_packed_txs(j: &Value, height: u64, prevhash: &Hash) -> Result<PackedTxs, String> {
    if find_u64(j, "ret") != Some(0) {
        let err = find_str(j, "err")
            .or_else(|| find_str(j, "http_error"))
            .unwrap_or_else(|| j.to_string());
        return Err(format!(
            "the node would not serve its pending block ({err})"
        ));
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
    let Some(bodies_json) = find_value(j, "transaction_body_list").and_then(|v| v.as_array())
    else {
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
            return Err(
                "a transaction body in the node's pending block is not a string".to_string(),
            );
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
/// `pin` carries the header timestamp a PREVIOUS run of this pool already handed
/// out, read back off the state file, so a restart part-way through a height
/// serves the same header bytes rather than silently invalidating every worker's
/// in-flight scan pass. It is only consulted when `current` is `None`: while the
/// pool is running the live template is the pin, and a fresh template for the
/// same height and parent is discarded by the caller anyway.
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
    pin: Option<&StampPin>,
) -> Option<(Template, Option<String>)> {
    let live = current.map(|t| StampPin {
        height: t.height,
        prevhash: t.prevhash.clone(),
        timestamp: t.timestamp,
    });
    let pin = live.as_ref().or(pin);
    let mut tpl = fetch_template_pinned(client, base, coinbase_addr, params, pin)?;
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
pub fn coinbase_with_extranonce(
    tpl: &Template,
    extranonce: &[u8; 32],
) -> mint::TransactionCoinbase {
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
pub fn submit_block_bytes(client: &reqwest::blocking::Client, base: &str, bytes: &[u8]) -> String {
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
    transactions
        .push(Box::new(cbtx.clone()))
        .expect("push coinbase");
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

    let block = BlockV1 {
        intro,
        transactions,
    };
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

    /// The exact figures a rig measured when a pool was restarted inside height
    /// 350: the pool already had 1785252287 in flight, and the parent block on the
    /// chain carried 1785252219, 68 seconds earlier.
    const RIG_HEIGHT: u64 = 350;
    const RIG_PREV_TS: u64 = 1785252219;
    const RIG_STAMP: u64 = 1785252287;

    fn rig_parent() -> Hash {
        Hash::from([0x5au8; 32])
    }

    fn rig_pin() -> StampPin {
        StampPin {
            height: RIG_HEIGHT,
            prevhash: rig_parent(),
            timestamp: RIG_STAMP,
        }
    }

    #[test]
    fn a_restart_inside_a_height_serves_the_same_header_timestamp() {
        // The pool pins one template per height and `/query/miner/notice` signals
        // only a HEIGHT change, so nothing tells a worker that the bytes under it
        // moved. A restart that re-stamps the template therefore invalidates every
        // connected worker's in-flight scan pass in silence, and every share found
        // during it is thrown away.
        //
        // Restart 68 seconds later, at the same height on the same parent: the
        // stamp already in flight is served again, so the 89-byte header is
        // byte-identical and nobody's work is lost.
        assert_eq!(
            template_timestamp(
                Some(&rig_pin()),
                RIG_HEIGHT,
                &rig_parent(),
                RIG_PREV_TS,
                RIG_STAMP + 68,
            ),
            RIG_STAMP
        );
        // With no pin - a first run, or a state file that predates the stamp -
        // nothing changes from what the pool always did.
        assert_eq!(
            template_timestamp(None, RIG_HEIGHT, &rig_parent(), RIG_PREV_TS, RIG_STAMP + 68),
            RIG_STAMP + 68
        );
    }

    #[test]
    fn a_pinned_stamp_is_refused_unless_it_belongs_to_this_exact_block() {
        // The guards here are not symmetric with the one above. Ignoring a good
        // pin costs one height of in-flight worker work and heals itself; honouring
        // a WRONG one puts a timestamp in a real block, and chain::verify rejects a
        // block whose timestamp is <= its parent's or in the future. /submit/block
        // answers before it validates, so such a block is refused silently and its
        // whole reward - the round every miner in the window is being paid from -
        // is gone.
        let fresh_now = RIG_STAMP + 68;

        // The chain moved on: this pin describes the previous block.
        assert_eq!(
            template_timestamp(
                Some(&rig_pin()),
                RIG_HEIGHT + 1,
                &rig_parent(),
                RIG_PREV_TS,
                fresh_now,
            ),
            fresh_now
        );
        // Same height, different parent: a same-height reorg. The old stamp was
        // only ever checked against the parent it was built on.
        assert_eq!(
            template_timestamp(
                Some(&rig_pin()),
                RIG_HEIGHT,
                &Hash::from([0xa5u8; 32]),
                RIG_PREV_TS,
                fresh_now,
            ),
            fresh_now
        );
        // A stamp at or below the parent's: the node refuses the block outright.
        let stale = StampPin {
            timestamp: RIG_PREV_TS,
            ..rig_pin()
        };
        assert_eq!(
            template_timestamp(
                Some(&stale),
                RIG_HEIGHT,
                &rig_parent(),
                RIG_PREV_TS,
                fresh_now
            ),
            fresh_now
        );
        // A stamp LATER than a fresh fetch would produce. A pin may only ever move
        // the header back to bytes this pool already served, never forward into a
        // future the node would reject.
        let ahead = StampPin {
            timestamp: fresh_now + 3_600,
            ..rig_pin()
        };
        assert_eq!(
            template_timestamp(
                Some(&ahead),
                RIG_HEIGHT,
                &rig_parent(),
                RIG_PREV_TS,
                fresh_now
            ),
            fresh_now
        );
    }

    /// The predicate the settlement guard used before this fix: a hash was kept
    /// only while the node reported it in the mempool. Every test below shows a
    /// case where it says "resolved" and the payout is in fact still undoable.
    fn old_guard_kept_the_hash(j: &Value) -> bool {
        find_u64(j, "ret") == Some(0) && j.get("pending").and_then(|v| v.as_bool()).unwrap_or(false)
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
        // An address holding nothing comes back as "0:0", which is a real zero.
        assert_eq!(balance_units("0:0"), Some(0));
        // Anything past the plausibility ceiling is a corrupt answer, not money.
        assert_eq!(
            balance_units(&format!("{MAX_PLAUSIBLE_UNITS}:247")),
            Some(MAX_PLAUSIBLE_UNITS)
        );
        assert_eq!(
            balance_units(&format!("{}:247", MAX_PLAUSIBLE_UNITS + 1)),
            None
        );
    }

    #[test]
    fn a_node_that_does_not_answer_is_not_a_zero_balance() {
        // What it costs when this is wrong: the pool publishes matured = 0 and
        // flags it CURRENT, /earnings tells every miner it is owed nothing for
        // the whole outage, and the miners whose shares leave the PPLNS window
        // while the node is away are never paid for that work.
        //
        // Every body get_json can hand the balance reader is here. Only the last
        // two are the node reporting a wallet.

        // The node is down: get_json wraps the transport error.
        let down = serde_json::json!({"http_error":"error sending request: connection refused"});
        assert!(matches!(balance_answer(&down), BalanceAnswer::NoAnswer(_)));
        assert_eq!(balance_answer(&down).units(), None);

        // Wrong port, or a proxy in front: a body that is not JSON at all.
        let html = Value::String("<html><body>502 Bad Gateway</body></html>".into());
        assert!(matches!(balance_answer(&html), BalanceAnswer::NoAnswer(_)));
        assert_eq!(balance_answer(&html).units(), None);

        // The node answered, and the answer is not a balance.
        let refused = serde_json::json!({"ret":1,"errmsg":"address format invalid"});
        assert!(matches!(
            balance_answer(&refused),
            BalanceAnswer::Refused(_)
        ));
        assert_eq!(balance_answer(&refused).units(), None);

        // ret=0 but no `hacash` field: not a node this pool understands.
        let odd = serde_json::json!({"ret":0,"list":[{"diamond":0}]});
        assert!(matches!(balance_answer(&odd), BalanceAnswer::Refused(_)));
        assert_eq!(balance_answer(&odd).units(), None);

        // A wallet holding nothing. The node renders it "0:0", and that IS a
        // balance: settlement must go on treating it as a real, actionable zero,
        // or an empty pool wallet would freeze payouts forever.
        let empty = serde_json::json!({"ret":0,"list":[{"hacash":"0:0"}]});
        assert_eq!(
            balance_answer(&empty),
            BalanceAnswer::Reported("0:0".into())
        );
        assert_eq!(balance_answer(&empty).units(), Some(0));

        // A funded wallet values exactly as it always did.
        let funded = serde_json::json!({"ret":0,"list":[{"hacash":"1:248"}]});
        assert_eq!(balance_answer(&funded).units(), Some(10));

        // The two halves of the old bug, asserted where they lived. The reader
        // was `find_str(&j, "hacash").unwrap_or_default()`, so every failure
        // above collapsed to ""; and "" was then valued as a confident zero.
        for bad in [&down, &html, &refused, &odd] {
            assert_eq!(find_str(bad, "hacash"), None);
        }
        assert_eq!(balance_units(""), None);
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
        assert_eq!(
            admission_of(&j(r#"{"ret":0,"pending":true}"#)),
            Admission::Held
        );
        assert_eq!(
            admission_of(&j(&format!(
                r#"{{"ret":0,"confirm":{PAYOUT_MATURITY_DEPTH}}}"#
            ))),
            Admission::Held
        );
        // Mined but shallow is still the node holding it.
        assert_eq!(
            admission_of(&j(r#"{"ret":0,"confirm":1}"#)),
            Admission::Held
        );
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
        assert_eq!(
            admission_of(&Value::String("<html>502</html>".into())),
            Admission::Unresolved
        );
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
    fn a_transaction_fee_is_valued_finer_than_a_payout_unit_and_always_upwards() {
        // The chain's own rendering: 1 HAC, 0.1 HAC (one payout unit), 0.01 HAC.
        assert_eq!(fin_fine_ceil("1:248"), Some(1_000_000_000));
        assert_eq!(fin_fine_ceil("1:247"), Some(100_000_000));
        assert_eq!(fin_fine_ceil("1:246"), Some(10_000_000));
        assert_eq!(fin_fine_ceil("0:0"), Some(0));
        // A real transaction fee is a small fraction of a payout unit. Valuing
        // it at payout-unit granularity would floor every one of them to nothing
        // and hand the whole of a block's fee income out at zero confirmations,
        // so it is summed on a scale that can actually hold it.
        assert_eq!(fine_to_units_ceil(fin_fine_ceil("1:246").unwrap()), 1);
        // Rounding is always UP, because this figure is money the pool refuses
        // to pay yet: a fee smaller than the finest step still counts as one.
        assert_eq!(fin_fine_ceil("1:230"), Some(1));
        assert_eq!(fin_fine_ceil("1:2"), Some(1));
        assert_eq!(fine_to_units_ceil(1), 1);
        assert_eq!(fine_to_units_ceil(0), 0);
        assert_eq!(fine_to_units_ceil(100_000_001), 2);
        // Not amounts at all. Each one must refuse rather than read as a zero
        // fee, which is what lets the fee be distributed unheld.
        assert_eq!(fin_fine_ceil(""), None);
        assert_eq!(fin_fine_ceil("12"), None);
        assert_eq!(fin_fine_ceil("-1:248"), None); // no such thing as a negative fee
        assert_eq!(fin_fine_ceil("x:248"), None);
        assert_eq!(fin_fine_ceil("1:256"), None); // the chain's unit is a u8
        assert_eq!(fin_fine_ceil("1:999"), None);
        // Larger than the whole coin supply: a corrupt or hostile answer, and
        // believing it would hold back every unit the pool will ever earn.
        assert_eq!(fin_fine_ceil("999999999:255"), None);
    }

    #[test]
    fn only_a_definitive_node_answer_prices_our_blocks_fees() {
        let j = |s: &str| serde_json::from_str::<Value>(s).expect("json");
        let ours = "aa".repeat(32);
        let theirs = "bb".repeat(32);
        // Our block, with two transactions to price.
        assert_eq!(
            block_txs_of(
                &j(&format!(
                    r#"{{"ret":0,"hash":"{ours}","tx_hash_list":["11","22"]}}"#
                )),
                &ours
            ),
            BlockTxs::Ours(vec!["11".to_string(), "22".to_string()])
        );
        // Our block, carrying nothing but its coinbase: no fees, and that is a
        // real answer rather than a refusal.
        assert_eq!(
            block_txs_of(
                &j(&format!(r#"{{"ret":0,"hash":"{ours}","tx_hash_list":[]}}"#)),
                &ours
            ),
            BlockTxs::Ours(vec![])
        );
        // Another block won that height, or the chain has not reached it: it
        // credited this pool nothing, so there are no fees to hold back.
        assert_eq!(
            block_txs_of(
                &j(&format!(
                    r#"{{"ret":0,"hash":"{theirs}","tx_hash_list":[]}}"#
                )),
                &ours
            ),
            BlockTxs::NotOnChain
        );
        assert_eq!(
            block_txs_of(&j(r#"{"ret":1,"err":"cannot find block"}"#), &ours),
            BlockTxs::NotOnChain
        );
        // Everything else is UNKNOWN, and the caller must stop settling. Reading
        // any of these as "no fees" pays a block's fee income out at zero
        // confirmations, and an orphan then leaves the operator funding it.
        for not_an_answer in [
            r#"{"http_error":"connection refused"}"#,
            r#"{"hash":"x"}"#,
            &format!(r#"{{"ret":0,"hash":"{ours}"}}"#), // no list: an unknown shape
            &format!(r#"{{"ret":0,"hash":"{ours}","tx_hash_list":[7]}}"#),
        ] {
            assert!(
                matches!(block_txs_of(&j(not_an_answer), &ours), BlockTxs::Unknown(_)),
                "{not_an_answer} was treated as an answer"
            );
        }
        assert!(matches!(
            block_txs_of(&Value::String("<html>502</html>".into()), &ours),
            BlockTxs::Unknown(_)
        ));

        // And the per-transaction half: `fee_got` is what the transaction really
        // paid for its place in the block, which is what the chain hands the
        // coinbase address.
        assert_eq!(
            fee_got_fine(&j(r#"{"ret":0,"fee":"2:246","fee_got":"1:246"}"#)),
            Some(10_000_000)
        );
        assert_eq!(fee_got_fine(&j(r#"{"ret":0,"fee":"1:246"}"#)), None);
        assert_eq!(fee_got_fine(&j(r#"{"ret":1,"err":"not found"}"#)), None);
        assert_eq!(
            fee_got_fine(&j(r#"{"http_error":"connection refused"}"#)),
            None
        );
    }

    #[test]
    fn a_durable_write_reports_whether_it_was_really_made_durable() {
        // What this costs when it goes wrong: `durable` is the promise the
        // settlement path broadcasts a payout on. The fsync result used to be
        // thrown away, so a full disk or a network mount that went away - both
        // of which surface at flush time and nowhere else - returned Ok, and the
        // payout went out against a state file that existed only in the page
        // cache. A power cut there loses the record of a signed transaction and
        // the next cycle pays the same window again.
        let path = tmp_path("durable.state.json");
        let _ = std::fs::remove_file(&path);
        atomic_write(&path, b"recorded", true).expect("durable write");
        assert_eq!(std::fs::read(&path).expect("read"), b"recorded");
        // The temp file is renamed, never left behind pointing at half a state.
        assert!(!std::path::Path::new(&format!("{path}.tmp.{}", std::process::id())).exists());

        // A bare filename is what a relative `wallet_file` in the config
        // produces. The directory fsync must fall back to the working directory
        // rather than fail: a durable write that always fails is a pool that
        // refuses to pay anyone at all.
        assert!(fsync_parent_dir("hbit-pool-no-such.state.json").is_ok());

        // And a write that cannot happen is still an error, not a quiet success:
        // this path has a FILE where it needs a directory.
        assert!(atomic_write(&format!("{path}/nested.json"), b"x", true).is_err());
        let _ = std::fs::remove_file(&path);
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
        // The hold-back survives, and an entry written before the pool held back
        // transaction fees reads as "fees NOT counted": that file really does
        // carry the subsidy alone, and reading it the other way would pay the
        // block's fees out on the first settlement after the upgrade.
        assert_eq!(
            load_immature_blocks(&path),
            vec![ImmatureBlock {
                height: 9,
                hash: "ab".to_string(),
                units: 30,
                fees_counted: false,
            }]
        );
        // The payout tool writes the SAME ledger, without losing the accounting.
        save_pending_payout_txs(&path, &["cafe".to_string()]).expect("save ledger");
        assert_eq!(load_pending_payout_txs(&path), vec!["cafe".to_string()]);
        let j: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(j["accepted"].as_u64(), Some(7));
        assert_eq!(j["order"].as_array().map(|a| a.len()), Some(2));
        assert_eq!(
            load_immature_blocks(&path)
                .iter()
                .map(|b| b.units)
                .sum::<u64>(),
            30
        );
        // The share window is readable from the same file, so the payout tool can
        // still settle correctly with the pool server stopped. This file is in the
        // OLD shape, written before shares were timed: it must still read, and
        // every share in it must weigh the same, or upgrading the pool would
        // reshuffle who is owed what.
        let credit = load_pplns_credit(&path);
        assert_eq!(credit.len(), 2);
        assert_eq!(credit[0].1, credit[1].1);
        assert!(credit[0].1 > 0);
        assert_eq!(
            credit.iter().map(|(w, _)| w.clone()).collect::<Vec<_>>(),
            vec!["a".to_string(), "b".to_string()]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_share_window_written_before_shares_were_timed_still_reads() {
        // The accounting file is the only record of who is owed what when the
        // server is stopped. A pool upgraded in place has one on disk in the OLD
        // shape - bare worker ids, no arrival times - and failing to read it does
        // not lose money quietly: it pays the whole balance to whoever mines
        // next.
        let old = serde_json::json!({"order": ["a", "b", "a"]});
        let rows = parse_share_order(&old, 5_000);
        assert_eq!(
            rows,
            vec![
                ("a".to_string(), 5_000),
                ("b".to_string(), 5_000),
                ("a".to_string(), 5_000),
            ],
            "every share in an untimed file must weigh the same"
        );
        assert!(parse_banked_credit(&old).is_empty());

        // The shape the pool writes now: (worker, arrival time in ms).
        let new = serde_json::json!({
            "order": [["a", 1_000], ["b", 2_500]],
            "banked": [{"at": 3, "rows": [["c", 700]]}],
        });
        assert_eq!(
            parse_share_order(&new, 5_000),
            vec![("a".to_string(), 1_000), ("b".to_string(), 2_500)]
        );
        assert_eq!(
            parse_banked_credit(&new),
            vec![(3u64, vec![("c".to_string(), 700u64)])]
        );

        // A row the file cannot describe is dropped, never guessed at, and a
        // missing time falls back rather than reading as time zero (which would
        // hand that share the maximum credit the horizon allows).
        let ragged = serde_json::json!({"order": [["a"], ["b", 9], 7, ["c", null]]});
        assert_eq!(
            parse_share_order(&ragged, 5_000),
            vec![("b".to_string(), 9), ("c".to_string(), 5_000)]
        );
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
            body_hex: "beef".to_string(),
            rows: vec![("w1".to_string(), 12), ("w2".to_string(), 3)],
        }];
        let mut paid = PaidLedger::started(1_000);
        let owed = vec![("w3".to_string(), 8u64)];
        save_settlement_ledger(&path, &["aa11".to_string()], &records, &owed, &paid).expect("save");

        // Everything else in the file is untouched.
        let j: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(j["accepted"].as_u64(), Some(3));
        assert_eq!(load_pending_payout_txs(&path), vec!["aa11".to_string()]);
        assert_eq!(load_payout_records(&path), records);
        // The signed bytes travel with the hash: without them a payout the node
        // relayed and then forgot can only be re-signed, which pays twice.
        assert_eq!(load_payout_records(&path)[0].body_hex, "beef");
        // What a failed chunk owes survives the same write, and the pool server
        // and the manual tool read it from the same place.
        assert_eq!(load_owed(&path), owed);

        // Confirm it, save again, and reload: the money is in the paid ledger and
        // out of the in-flight rows, in both memory and on disk.
        confirm_payout(&mut records, &mut paid, "aa11", 1_200).expect("credited");
        save_settlement_ledger(&path, &[], &records, &owed, &paid).expect("save");
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
            std::fs::read_to_string(&path)
                .expect("owner can still read")
                .trim(),
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
        assert!(
            !body.contains(key_hex),
            "the key must not survive in the clear"
        );
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
        assert_eq!(
            *intro.head.transaction_count, 4,
            "coinbase plus 3 packed txs"
        );
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
        assert_eq!(
            intro.head.mrklroot,
            calculate_mrklroot(&vec![cb.hash_with_fee()])
        );
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
        assert_eq!(
            got.block_tx_count(),
            3,
            "our coinbase plus the node's two txs"
        );
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
        assert!(
            parse_node_packed_txs(&pending_reply(51, &prev, &bodies, &sib), 50, &prev).is_err()
        );
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
