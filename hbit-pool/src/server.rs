//! Hacash pool server: serves work, validates shares, keeps PPLNS accounting,
//! submits full blocks, and settles payouts. Blocking HTTP on std::net - no
//! async runtime, no node changes.
//!
//! Speaks the STANDARD miner API (so an unmodified poworker can mine here) with
//! one difference: `target_hash` carries the pool's SHARE target. A submission
//! is promoted to a real block whenever it also beats the network target.
//!
//! Protections that matter once other people's hashrate is involved:
//!   * duplicate shares are rejected (a resubmitted solution cannot inflate a
//!     miner's PPLNS credit at everyone else's expense)
//!   * accounting is persisted atomically, so a restart never erases work
//!   * a submitted block only counts once the chain still holds OUR hash at that
//!     height - orphans are detected and not paid for
//!   * a found block's coinbase is held back from settlement until the chain has
//!     buried it, so a reorg can never revoke income that was already paid out
//!   * only one process may settle a wallet (OS lock), and it shares ONE pending
//!     payout ledger with the manual hbit-pool-payout tool
//!   * settlement runs automatically on a timer, is idempotent across restarts,
//!     and chunks into <=200-action transactions the node will accept
//!   * one panicking request or poisoned lock cannot freeze or crash the pool
//!   * per-IP connection caps + a separate long-poll budget stop one host from
//!     exhausting every connection slot
//!
//! Endpoints: /work, /share, /stats, /terms, /earnings (own protocol) and
//! /query/miner/pending, /query/miner/notice, /submit/miner/success (standard
//! API).
//!
//! /terms and /earnings exist so a miner can judge the pool and see its money:
//!   * /terms states the scheme, window, fee, minimum payout and maturity depths
//!     by READING the constants and live values the pool acts on, so what is
//!     advertised cannot drift from what is done
//!   * /earnings?worker=<address> reports one worker's shares, its PAID total
//!     (confirmed on chain), what is IN FLIGHT (submitted, unconfirmed) and an
//!     explicitly-labelled PENDING estimate. The three are disjoint, and any of
//!     them the pool cannot stand behind is reported as unknown, never as zero.
//!
//! Usage: hbit-pool-server <node> <wallet_file> <listen> <share_bits> <chain> [settle_secs]
//!   `--help` prints the whole of it: every argument, what it means, and a
//!   working example. See `usage()`, which is the only place that text lives.
//!   All five positional arguments are REQUIRED and none is guessed. `chain`
//!   above all: a wrong difficulty rule makes every share and block one the node
//!   rejects, so it is `mainnet`, `testnet`, or
//!   `testnet:<difficulty_adjust_blocks>:<each_block_target_time>` for a testnet
//!   node configured with anything other than the documented 288/10 pair, and
//!   the choice is PROVED against the node's own tip before work is served.
//!
//! Nothing here ever prompts. It has to come up unattended under a service
//! manager, so every input is an argument or an environment variable, and
//! anything missing or wrong is a refusal that says what to do about it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use basis::interface::*;
use field::*;
use protocol::action::HacToTrs;
use protocol::transaction::TransactionType2;
use sys::{Account, curtimes};

use hbit_pool::difficulty::ChainParams;
use hbit_pool::pool_core::{self, Pplns, split_payout};
use hbit_pool::{
    Admission, BalanceAnswer, BlockFees, DEFAULT_SETTLE_SECS, GoneAction, PAYOUT_CHUNK,
    PAYOUT_DUST_UNITS, PAYOUT_MATURITY_DEPTH, PAYOUT_UNIT, POOL_FEE_UNITS, PPLNS_WINDOW,
    PaidLedger, PayoutRecord, PayoutTxState, SETTLE_RESERVE_UNITS, StampPin, SubmitVerdict,
    Template, WALLET_PASSWORD_ENV, WALLET_PASSWORD_FILE_ENV, acquire_settle_lock, assemble_block,
    atomic_write, balance, block_fees, block_reward_units, chunk_tx_fee, classify_payout_tx,
    coinbase_body_hex, coinbase_with_extranonce, confirm_payout, deduct_owed, distributable_units,
    drop_payout, fetch_pool_template, find_str, find_u64, get_json, gone_action, http_client,
    intro_bytes, is_payout_address, load_or_create_wallet, merge_payout_rows, owe_rows,
    owed_to_json, parse_banked_credit, parse_owed, parse_paid_ledger, parse_payout_records,
    parse_share_order, payout_amount, pool_state_path, post_hex, pplns_horizon_ms,
    settle_lock_path, submit_block_bytes, submit_verdict, take_owed, verify_admitted,
    verify_chain_params,
};

use serde_json::json;
use zeroize::Zeroizing;

/// Global live-connection cap and the per-source-IP cap. The per-IP cap is what
/// actually stops one host pinning every slot with long-polls; the global cap is
/// a coarse backstop.
const MAX_CONNS: usize = 1024;
const MAX_PER_IP: u32 = 24;
/// Long-poll waiters are budgeted separately so they can never consume the whole
/// connection pool and starve short /work, /share and /submit requests.
const MAX_NOTICE_WAITERS: usize = 384;
/// Per-height replay-protection set bound: beyond this a template is producing an
/// implausible flood, so reject further shares this height rather than grow memory
/// without limit. Reset every time the height advances.
const SEEN_CAP: usize = 2_000_000;
/// How many BLOCKS an outstanding payout may go unresolved before the routine
/// note escalates to a loud warning: nothing is being paid, and the operator has
/// to know why.
///
/// Measured in blocks, never in settlement cycles. A payout needs one block to
/// include it and `PAYOUT_MATURITY_DEPTH - 1` more to bury it, so it is in
/// flight for about `PAYOUT_MATURITY_DEPTH` blocks BY DESIGN, while the settle
/// interval is an operator setting with no relation to block time. Counted in
/// cycles, mainnet defaults (300s blocks, a 300s settle interval) fire the alarm
/// on cycles 3-6 of every completely healthy payout, forever. What that costs:
/// the operator learns the warning means nothing, and the day a payout really is
/// stuck - freezing every later payout behind it, so nobody is paid at all - the
/// line saying so reads exactly like the noise they have been ignoring. Twice
/// the burial depth leaves room for ordinary block-time variance.
const STALLED_PAYOUT_BLOCKS: u64 = 2 * PAYOUT_MATURITY_DEPTH;
/// Wall-clock safety factor for the same check. A tip that has stopped moving is
/// the one stall a block count can never see - zero blocks elapse, forever - so
/// the wait is ALSO measured against what `STALLED_PAYOUT_BLOCKS` blocks should
/// take at the chain's target block time. Block times are exponentially
/// distributed, so a run of slow blocks is ordinary; the factor keeps that
/// variance out of the alarm and leaves it firing only on a chain that has
/// genuinely stopped delivering blocks to this node.
const STALLED_PAYOUT_TIME_SLACK: u64 = 2;
/// How deep one of OUR blocks must be buried before its coinbase may be paid
/// out. The node treats anything shallower than `unstable_block` (4) as
/// reorg-able, and settlement runs at roughly one block interval, so without a
/// generous margin the pool would distribute income that is 0-1 confirmations
/// old. If such a block is later orphaned the income vanishes from the canonical
/// chain while the payout that spent it stays valid, and the operator eats the
/// whole subsidy with no way to recover it.
const COINBASE_MATURITY_DEPTH: u64 = 16;
/// Absolute wall-clock budget for reading a request line. A socket read timeout
/// bounds each read syscall, NOT the whole request, so a client dribbling one
/// byte at a time never trips it; this deadline is what actually stops it.
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(5);
/// Longest request line accepted. A miner API request line is tiny.
const MAX_REQUEST_LINE: usize = 4 * 1024;
/// Stack for a connection handler. HTTP parsing plus one x16rs evaluation needs
/// well under 100 KB, so this trims the address space reserved by MAX_CONNS
/// live handlers without coming close to the real requirement.
const HANDLER_STACK_BYTES: usize = 1024 * 1024;
/// Narrowest and widest share the pool will serve, as powers of two easier than
/// a network block. See `check_share_factor` for why both ends matter.
const MIN_SHARE_FACTOR: u32 = 18;
const MAX_SHARE_FACTOR: u32 = 40;
/// The least work one share may cost, as a power of two hashes.
///
/// `MIN_SHARE_FACTOR` bounds a RATIO - how much easier a share is than a block -
/// and a ratio says nothing about what the share itself costs. Both bounds are
/// needed. A chain whose target has 22 leading zero bits, served with
/// `share_bits` 24, saturates to the all-0xff ceiling: the achieved factor reads
/// 22, clears the ratio bound, and every hash on earth beats the share target.
///
/// 2^16 is the floor at which a share is unambiguously work rather than a round
/// trip: even a single CPU thread produces well under one per second, so credit
/// tracks hashing. Real mainnet difficulty is far above this at any legal
/// `share_bits`, so this bound only ever bites on a chain too easy to account on.
const MIN_SHARE_COST_BITS: u32 = 16;
/// Consecutive above-target submissions from one worker before the pool shouts.
/// A worker that hashes the same header the pool reconstructs essentially never
/// produces one, because it only submits what already beat the target it was
/// handed. A steady stream of them means the two are hashing DIFFERENT headers,
/// and every one of that worker's shares is being thrown away.
const BAD_STREAK_WARN: u64 = 16;
/// Workers tracked by the above-target streak counter. Bounded so a flood of
/// invented worker ids cannot grow memory; the diagnostic is for real miners.
const BAD_STREAK_WORKERS: usize = 4096;
/// Quiet time between repeats of the above-target streak line, PER WORKER.
///
/// It used to repeat at every multiple of `BAD_STREAK_WARN`, which is once per 16
/// rejects. A pool restart mid-height changes the header under every connected
/// worker, and a rig measured 6,320 consecutive rejects from one worker while it
/// finished the scan pass it was already in: that is 395 copies of the same line
/// from one worker, and 1,281 copies were logged across the fleet. A diagnostic
/// nobody can read past is a diagnostic nobody reads.
const BAD_STREAK_REPEAT_MS: u64 = 300_000;
/// How long after the pool last changed the header it serves an above-target
/// streak is reported as the expected cost of that change rather than as
/// something about the worker.
///
/// One mainnet block interval, which comfortably covers a GPU scan pass. The pool
/// pins one template per height and `/query/miner/notice` signals only a HEIGHT
/// change, so a worker legitimately keeps hashing the header it was handed until
/// its current pass ends. Nothing here decides whether the line is printed, only
/// which sentence follows it: neither wording accuses the worker of anything.
const TEMPLATE_SETTLE_MS: u64 = 300_000;
/// Quiet time between the accepted-share summary lines on stdout.
///
/// The pool used to print one line per accepted share. That is invisible at
/// mainnet difficulty, where one worker finds a share every few seconds, and it
/// is a flood at the pool's own minimum `share_bits` on an easy chain: a rig
/// measured 7.6 MB in `pool.out.log` in four minutes, roughly a thousand lines a
/// second, over 3.3 million shares. Nothing rate limited it.
///
/// What that costs is not disk. The block-found notice went out through the SAME
/// println, so the one line an operator must never miss - the pool earned money,
/// or the node refused the block and the reward is gone - was buried in thousands
/// of identical lines a second. A log nobody can read past is a log nobody reads,
/// and the operator is then flying blind on other people's money.
///
/// Ten seconds is short enough that "shares are arriving" is still a live signal
/// and long enough that a human can read every line the pool prints.
const SHARE_LOG_EVERY_MS: u64 = 10_000;
/// Distinct workers one summary line tracks. Past this the line reports "at
/// least N", because this set is grown on the share hot path under the pool
/// lock: an unbounded one is a flood of invented payout addresses away from
/// being a memory leak every miner's request is serialized behind.
const SHARE_LOG_WORKERS: usize = 64;
/// How many of those workers the line NAMES before it just counts the rest. The
/// whole point of the summary is a line an operator can read.
const SHARE_LOG_NAMES: usize = 4;
/// How long the "mining without the node's transactions" warning is suppressed
/// after being printed with the same reason. The template loop runs every couple
/// of seconds, so without this the warning would be the whole log.
const TX_WARN_REPEAT: Duration = Duration::from_secs(300);
/// Template cycles between refreshes of the pool's own wallet valuation. That
/// number is what `/earnings` divides into a worker's PENDING estimate, and the
/// settlement timer is far too slow to keep it current on its own. The cycle
/// runs every two seconds, so this is roughly half a minute: one extra
/// `/query/balance` per 30s against the node, and never on a miner's request
/// path.
const MONEY_REFRESH_CYCLES: u64 = 15;
/// Template cycles a submitted block may go unnoticed by the chain before the
/// pool shouts. `/submit/block` validates ASYNCHRONOUSLY and answers before the
/// verdict, so the only evidence a block was refused is that the tip never
/// reaches its height. At roughly one cycle every two seconds this is about two
/// minutes, far longer than a node needs to insert a block it accepted.
const BLOCK_STALL_CYCLES: u32 = 60;
/// Bounds on the automatic settlement interval. Each settlement is a signed
/// on-chain transaction carrying a network fee, so running one every few seconds
/// spends the reserve for nothing; `0` is worse still, because the timer thread
/// would then never sleep and would hammer the node in a tight loop. The upper
/// bound is a day: past that the operator has effectively turned payouts off and
/// should say so by stopping the pool, not by typing a large number.
const MIN_SETTLE_SECS: u64 = 30;
const MAX_SETTLE_SECS: u64 = 86_400;
/// The documented default share size. `usage()` quotes it, so the help text
/// cannot describe a default the code does not use. The settlement interval's
/// default lives beside the credit horizon it sizes, in `hbit_pool`.
const DEFAULT_SHARE_BITS: u32 = 24;
/// The largest hashrate the pool will believe from ONE worker id, as a power of
/// two hashes per second. Deliberately far above any real x16rs farm: this is a
/// ceiling on the absurd, not a throttle on big miners.
///
/// It exists because credit is per-share and a share costs a known amount of
/// work: submitting faster than this ceiling divided by that cost is claiming
/// hashing that no hardware on the chain performed. Without it one worker can
/// insert an unbounded burst of withheld shares in the second before a
/// settlement, which is the whole of the withholding attack.
const MAX_WORKER_HASHRATE_LOG2: u32 = 48;
/// Seconds of that ceiling a worker may save up and spend at once, so ordinary
/// batch jitter (a GPU finishing several shares in one kernel) is never refused.
const WORKER_BURST_SECS: u64 = 30;
/// ...and never fewer shares than this, whatever one share costs. A chain where
/// a single share costs more than the ceiling above produces per second would
/// otherwise leave every honest worker unable to submit at all.
const WORKER_BURST_MIN_SHARES: u64 = 64;
/// Workers tracked by the rate limiter. Bounded so a flood of invented payout
/// addresses cannot grow memory; entries that would have refilled to full are
/// pruned first, so pruning never hands anyone credit it should not have.
const RATE_WORKERS: usize = 100_000;
/// Shortest passphrase the wallet layer accepts. Mirrored here only so a short
/// one is a refusal that says what to do, instead of a panic out of the key
/// loader with a backtrace note on it. The loader still has the final say: if
/// these ever disagree, the worst case is the old panic, never a weaker key.
const WALLET_PASSWORD_MIN: usize = 8;

static CONNS: AtomicUsize = AtomicUsize::new(0);
static NOTICE_WAITERS: AtomicUsize = AtomicUsize::new(0);
static PER_IP: LazyLock<Mutex<HashMap<IpAddr, u32>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// Last printed reason (and when) for mining without the node's transactions.
static TX_WARN: LazyLock<Mutex<Option<(String, Instant)>>> = LazyLock::new(|| Mutex::new(None));
/// Submitted blocks the chain has not reached yet, and for how many cycles.
static BLOCK_STALL: LazyLock<Mutex<HashMap<(u64, [u8; 32]), u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Lock the pool, recovering from a poisoned mutex instead of cascading panics.
/// With panic=unwind a handler that panics under the lock poisons it; recovering
/// keeps the pool serving instead of turning one fault into permanent death.
fn plock(m: &Mutex<Pool>) -> MutexGuard<'_, Pool> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn per_ip_lock() -> MutexGuard<'static, HashMap<IpAddr, u32>> {
    PER_IP.lock().unwrap_or_else(|e| e.into_inner())
}

/// Releases a connection's global + per-IP slot on scope exit - including on an
/// unwind - so a panicking handler can never leak a slot and wedge the listener.
struct ConnGuard {
    ip: Option<IpAddr>,
}
impl Drop for ConnGuard {
    fn drop(&mut self) {
        CONNS.fetch_sub(1, Relaxed);
        if let Some(ip) = self.ip {
            let mut m = per_ip_lock();
            if let Some(c) = m.get_mut(&ip) {
                *c -= 1;
                if *c == 0 {
                    m.remove(&ip);
                }
            }
        }
    }
}

/// Releases a long-poll waiter slot on scope exit.
struct NoticeGuard;
impl Drop for NoticeGuard {
    fn drop(&mut self) {
        NOTICE_WAITERS.fetch_sub(1, Relaxed);
    }
}

/// A serialized accounting snapshot on its way to disk. Building it needs the
/// pool lock; WRITING it must not hold it. Every served endpoint takes the same
/// mutex, so a create/rename/fsync on a slow, full or networked disk would
/// otherwise freeze work distribution and share acceptance for every miner for
/// as long as the disk takes.
struct StateShot {
    seq: u64,
    path: String,
    body: Vec<u8>,
    durable: bool,
}

/// What has reached the state file already, so a writer that lost a race does no
/// work twice. Held only around the write itself, never together with the pool
/// lock.
#[derive(Default, Clone, Copy)]
struct Persisted {
    /// Newest snapshot whose bytes are in the file.
    seq: u64,
    /// Newest snapshot that was written WITH an fsync behind it.
    ///
    /// Kept apart from `seq` because the two are different promises and only one
    /// of them is worth money: a snapshot can be newer and still have been
    /// written without ever being flushed.
    durable_seq: u64,
}

static PERSIST: LazyLock<Mutex<Persisted>> = LazyLock::new(|| Mutex::new(Persisted::default()));

/// Write a snapshot to disk, OFF the pool lock. The file is written atomically
/// (temp + optional fsync + rename + directory fsync) by `hbit_pool::atomic_write`,
/// so a crash or a full disk mid-write can never leave a truncated or corrupt file.
///
/// Returns false only if this snapshot (or a newer one carrying the same promise)
/// did NOT reach disk, so a caller about to move money can refuse to proceed
/// rather than pay out untracked.
fn flush_state(shot: Option<StateShot>) -> bool {
    let Some(shot) = shot else {
        return true;
    };
    let mut last = PERSIST.lock().unwrap_or_else(|e| e.into_inner());
    // A snapshot may only stand on one that promised at least as much as it did.
    // Sequence alone is not that test. The share hot path saves every 16th share
    // WITHOUT an fsync on purpose, and its sequence is newer than a settlement
    // that took the pool lock first and reached this line second - the pool mutex
    // is not fair, so that ordering needs no unusual load. Short-circuiting on
    // sequence told the settlement "recorded" for a payout nothing had fsynced,
    // and the transaction was broadcast on that answer. A power cut in the
    // seconds after loses the pool's only record of a signed payout, and the next
    // cycle signs a SECOND one for the same PPLNS window out of the operator's
    // wallet.
    // A durable snapshot may only be waved through by another DURABLE one. A
    // newer non-durable share save promised less, so it cannot stand in.
    let already = if shot.durable {
        last.durable_seq
    } else {
        last.seq
    };
    if shot.seq <= already {
        // A snapshot at least this new AND at least this durable already landed.
        // It was taken under the pool lock after this one, so it carries
        // everything this one carried.
        return true;
    }
    if let Err(e) = atomic_write(&shot.path, &shot.body, shot.durable) {
        eprintln!("[state] save failed ({e}); accounting NOT flushed this round");
        return false;
    }
    // The file now holds THIS snapshot, even in the case where that steps back
    // over a newer non-durable one. What such a snapshot can carry that this one
    // cannot is only shares - every money change takes its own durable snapshot,
    // and a durable one that already landed short-circuits above - and those
    // shares are still in memory, bounded by the same 16 the debounced save
    // already accepts losing.
    last.seq = shot.seq;
    if shot.durable {
        last.durable_seq = shot.seq;
    }
    true
}

/// A valuation of the pool's own wallet, taken off the pool lock.
///
/// `units` is what has MATURED and is not yet settled: the node's confirmed
/// balance, minus block income a reorg could still revoke, minus the fee reserve.
/// It is deliberately a snapshot with a timestamp: `/earnings` must never make a
/// node call, and a miner reading a figure has to be able to see how old it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Matured {
    units: u64,
    at: u64,
}

/// Income from one block this pool found that a reorg could still take back.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Immature {
    height: u64,
    /// OUR block's hash at that height. The hold-back tracks THIS hash: the
    /// chain holding some other block there means the income never landed.
    hash: [u8; 32],
    /// What that block put into the pool wallet, in units of 0.1 HAC: the
    /// coinbase subsidy, plus the block's transaction fees once `fees_counted`.
    units: u64,
    /// Are this block's TRANSACTION FEES already inside `units`?
    ///
    /// They are a second, separate credit to the very same wallet: the chain
    /// pays the coinbase address the subsidy AND the sum of the fees of every
    /// transaction in the block, and this pool packs the node's transactions.
    /// While this is false, `units` is the subsidy alone, that fee income is
    /// sitting in the balance with nothing holding it back, and a settlement
    /// would hand it to miners at zero confirmations. If the block is then
    /// orphaned the money is gone from the chain while the payout that spent it
    /// stays valid, and the operator funds the difference out of their own
    /// wallet with no way to claw it back.
    ///
    /// It starts false because the figure is not knowable when the block is
    /// submitted: the packed transactions are raw bytes this pool has no codec
    /// for, and `/submit/block` reports only that it took the block. Settlement
    /// reads it back off the node before it values anything.
    fees_counted: bool,
}

/// `durable` fsyncs before the rename; the frequent debounced share-save skips it
/// (a crash loses at most the last handful of shares, which is already the
/// accepted tolerance).
struct Pool {
    node: String,
    payout: String,
    /// The key behind `payout`, loaded ONCE at startup and never read again.
    ///
    /// It is pinned here for the same reason the address above is, and the cost
    /// of re-reading it per settlement cycle was paid twice over. A read that
    /// failed for any reason OTHER than "no such file" ended the process from
    /// inside the settlement thread - on Windows that is an antivirus or backup
    /// holding the key file open for a few milliseconds - and being an exit
    /// rather than a panic, the `catch_unwind` wrapped around the cycle could
    /// not see it: the pool simply stopped, mid-flight, with payouts in the
    /// mempool. And a key file that was momentarily ABSENT was replaced by a
    /// fresh random wallet, after which the pool went on mining to `payout`
    /// while valuing and signing from an address no block had ever paid: every
    /// miner is then told a confidently current balance of zero, with no banner
    /// and no refusal, for as long as it lasts.
    ///
    /// `Arc` so a settlement can take it and release the pool lock before it
    /// signs; every miner request is serialized behind that same mutex.
    acc: Arc<Account>,
    state_file: String,
    client: reqwest::blocking::Client,
    params: ChainParams,
    tpl: Template,
    share_target: [u8; 32],
    /// How many powers of two EASIER than the network target a share is. The
    /// share target is derived from the live network difficulty (not an absolute
    /// value), so it scales as difficulty changes and a share represents a fixed
    /// fraction of a real block - which is what makes credit proportional to
    /// hashrate instead of to batch cadence.
    share_factor: u32,
    /// The factor the pool is REALLY serving. `share_target_hash` saturates at
    /// the all-0xff ceiling, so on a low-difficulty chain the target handed out
    /// can be far easier than `share_factor` asks for - and at the ceiling every
    /// hash is a share, which is exactly when credit stops tracking hashrate.
    /// Derived from the two targets, never assumed, and re-derived on every
    /// difficulty change.
    share_factor_achieved: u32,
    /// What one share actually COSTS on the target being served, as a power of
    /// two hashes. `share_factor_achieved` is a RATIO and clears its bound in the
    /// exact case that costs money: on a chain with 22 leading zero bits the
    /// achieved factor reads 22 while the target has saturated to all-0xff and
    /// every hash on earth is a share. Both figures have to be live, or the whole
    /// band of difficulty from 40 bits down to 18 passes in silence.
    share_cost_bits: u32,
    /// Why the pool has STOPPED crediting shares and STOPPED settling, if it has.
    ///
    /// Startup refuses a share target the live difficulty cannot support, but
    /// difficulty MOVES: the same condition arrives mid-run as a difficulty fall,
    /// with miners already connected. From that moment a share is not evidence of
    /// work, PPLNS credit measures how fast a worker can complete an HTTP round
    /// trip, and one worker in a loop takes the whole payout window from everyone
    /// else. The settlement that follows pays that worker other people's money and
    /// cannot be taken back. Re-derived from the live target on every template
    /// change, so it also clears by itself once the difficulty recovers.
    share_halt: Option<String>,
    network_target: [u8; 32],
    /// Cached /query/miner/pending response for the current template, rebuilt
    /// only when the template changes so a poll never rebuilds it under the lock.
    pending_cache: String,
    workers: HashMap<String, [u8; 32]>,
    next_en: u64,
    pplns: Pplns,
    accepted: u64,
    blocks: u64,
    orphaned: u64,
    /// Solutions already credited for the current template - rejects replays.
    seen: HashSet<(u64, [u8; 32], u32)>,
    /// Blocks we submitted, awaiting confirmation that they stuck.
    submitted: Vec<(u64, [u8; 32])>,
    /// Found blocks whose income is NOT yet safe to distribute. An entry leaves
    /// only once the chain still holds OUR hash COINBASE_MATURITY_DEPTH blocks
    /// later, or immediately once the chain shows a different hash there
    /// (orphaned, so that income never lands). Its income is held back from
    /// `distributable` until then, and it is persisted so a restart cannot
    /// forget the hold-back.
    immature: Vec<Immature>,
    /// Accepted shares not yet flushed to disk (debounces state writes).
    unsaved: u32,
    /// Monotonic snapshot counter: the state writer uses it to drop a snapshot
    /// that a fresher one has already overtaken.
    state_seq: u64,
    /// Hashes of payout transactions that have not yet confirmed. While ANY is
    /// still in the mempool we must not settle again (double spend). Persisted so
    /// a restart mid-settlement does not re-pay. Robust to a lost submit ACK and
    /// to the wallet also earning coinbase income.
    settle_pending_txs: Vec<String>,
    /// The exact per-recipient rows of every payout in `settle_pending_txs` this
    /// pool submitted itself. A hash alone can only say that SOME payout is in
    /// flight; these are what let a miner be told what is in flight FOR IT, and
    /// what let a confirmed payout be credited to the right people.
    payout_records: Vec<PayoutRecord>,
    /// Total in-flight units across `payout_records`. Derived, kept beside them
    /// so the PENDING estimate can subtract it in O(1): the node's CONFIRMED
    /// balance still contains money that is already in a submitted payout, so
    /// without this subtraction the same units would be reported as pending AND
    /// as in flight.
    inflight_units: u64,
    /// (worker address, units of 0.1 HAC) a settlement chunk was planned to pay
    /// and definitively did not. Persisted, and paid FIRST by the next cycle,
    /// before any fresh income is split.
    ///
    /// Without it the money in a failed chunk falls back into the pot and the
    /// next cycle re-splits it over the whole live window: the miners whose
    /// chunks DID go through take a second cut of the same window, and the
    /// miners in the failed chunk - who were paid nothing - are diluted by them.
    owed: Vec<(String, u64)>,
    /// What the pool has actually PAID each worker, folded in only when the node
    /// reports a payout buried. Persisted, and only ever grows.
    paid: PaidLedger,
    /// The pool's last successful valuation of its own wallet: what has matured
    /// and is not yet settled. `None` means the pool has never been able to value
    /// its balance, and PENDING must then be reported as unknown - never as zero.
    matured: Option<Matured>,
    /// Did the most recent valuation attempt succeed? A stale figure is still
    /// worth reporting, but a miner has to be told it is stale.
    matured_current: bool,
    /// How often the settlement timer runs, so `/terms` states the interval this
    /// process is actually using rather than a documented default.
    settle_secs: u64,
    /// The node's tip height and the unix second at which this pool FIRST failed
    /// to resolve the payouts it is still tracking. `None` once everything
    /// outstanding is buried or definitively gone.
    ///
    /// This is the baseline a stall is measured against, and it is a HEIGHT
    /// because a payout waits on blocks: it needs one block to include it and
    /// `PAYOUT_MATURITY_DEPTH - 1` more to bury it, whatever the settle interval
    /// happens to be. The second is kept beside it only so a chain whose tip has
    /// stopped moving - where no number of blocks ever elapses - is still caught.
    /// A payout that never confirms freezes every later payout, so a stall must
    /// be reported; reporting one that is not happening is worse than silence,
    /// because it sends the operator to fix something that is not broken.
    /// Diagnostic only, like the cycle counter it replaces, so not persisted.
    stall_since: Option<(u64, u64)>,
    /// Consecutive above-target submissions per worker, cleared by that worker's
    /// next accepted share. The pool never trusts a worker's own header: it
    /// rebuilds the header from (height, coinbase_nonce, block_nonce) and hashes
    /// that, so a worker computing a different merkle root is REJECTED rather
    /// than credited - it cannot steal, but it also cannot earn, and counting
    /// those rejects in silence is how a worker mines for nothing all day.
    bad_streak: HashMap<String, BadStreak>,
    /// When the pool last began serving DIFFERENT header bytes, including this
    /// process's own start. Restarting inside a height re-stamps the template, so
    /// a restart changes the header just as a new tip does.
    ///
    /// Read only by the above-target streak line, and it is what keeps that line
    /// honest: from the pool's side a worker still hashing the previous header and
    /// a worker that is genuinely broken look identical, so the message may not
    /// claim to tell them apart. It used to, and it was the pool's own restart
    /// that produced the condition it blamed on the worker.
    tpl_changed_at_ms: u64,
    /// Per-worker submission budget. Purely a bound on the absurd, so it is not
    /// persisted: a restart hands everyone a full bucket, which is the same
    /// position an honest worker is always in.
    rates: HashMap<String, ShareRate>,
    /// Accepted shares waiting to be reported as ONE line. Diagnostic only, so
    /// not persisted: a restart starts a fresh count and says so.
    share_log: ShareLog,
}

/// One worker's run of above-target submissions, and when the pool last said so.
///
/// The print time is per worker rather than global: a fleet where every rig is
/// affected must not have one rig's line silence all the others.
#[derive(Debug, Clone, Copy, Default)]
struct BadStreak {
    count: u64,
    /// `None` until the streak has been reported once.
    warned_at_ms: Option<u64>,
}

/// The accepted shares that have not been spoken for yet, folded into the one
/// line that will stand for all of them.
///
/// This exists because the alternative was measured: one println per accepted
/// share is ~1,000 lines a second at the pool's own minimum share_bits, and it
/// buried the block-found notice it shared a print with. Blocks do NOT come
/// through here - they are announced every single time, whatever the shares are
/// doing.
#[derive(Debug, Default)]
struct ShareLog {
    /// Shares accepted since the last line went out.
    pending: u64,
    /// The workers behind them, capped at `SHARE_LOG_WORKERS`. Sorted, so the
    /// same set of miners always reads the same way from one line to the next.
    workers: BTreeSet<String>,
    /// A worker turned up that the cap had no room for, so the headcount on the
    /// line is a floor and has to say so rather than under-report the fleet.
    capped: bool,
    /// Height of the most recent share folded in.
    height: u64,
    /// Pool clock when the last line went out. `None` until the first accepted
    /// share of the process, and that one prints IMMEDIATELY: someone bringing a
    /// new pool up has to see that work is arriving, and a pool that says
    /// nothing for the first ten seconds looks exactly like a pool nobody can
    /// mine on - which is how an operator ends up debugging a rig that is fine.
    last_ms: Option<u64>,
}

impl ShareLog {
    /// Fold in one accepted share. `Some` is the line to print, and the caller
    /// must print it with the pool lock RELEASED: stdout blocks on a slow
    /// console or a full pipe, and every other miner's request is serialized
    /// behind that same mutex.
    fn note(&mut self, worker: &str, height: u64, now_ms: u64) -> Option<String> {
        self.pending = self.pending.saturating_add(1);
        self.height = height;
        if self.workers.len() < SHARE_LOG_WORKERS {
            self.workers.insert(worker.to_string());
        } else if !self.workers.contains(worker) {
            self.capped = true;
        }
        self.due(now_ms)
    }

    /// The line owed right now, if one is owed.
    ///
    /// Also called on the template timer, so the last batch of shares before the
    /// miners go quiet is still counted out loud. Without that the count sits
    /// here until a share that may never come, and the operator's last word on a
    /// fleet that has just stopped is a stale line from before it stopped.
    fn due(&mut self, now_ms: u64) -> Option<String> {
        if self.pending == 0 {
            return None;
        }
        let span = match self.last_ms {
            None => None,
            // A system clock that steps BACKWARDS (ntp correction, a VM resumed
            // from a snapshot) must not silence the pool until it catches up:
            // read any negative span as due, and report it as zero elapsed
            // rather than as an enormous one.
            Some(at) if now_ms < at => Some(0),
            Some(at) if now_ms - at >= SHARE_LOG_EVERY_MS => Some(now_ms - at),
            Some(_) => return None,
        };
        let line = share_summary_line(self.pending, span, self.height, &self.workers, self.capped);
        self.last_ms = Some(now_ms);
        self.pending = 0;
        self.workers.clear();
        self.capped = false;
        Some(line)
    }
}

/// The one line that stands in for every share since the previous one.
///
/// It carries the count and the workers because those are the two things an
/// operator watching a pool has to be able to see: that work is arriving at all,
/// and who it is arriving from. A bare "shares are being accepted" would be no
/// better than the flood it replaces.
///
/// `span_ms` is `None` for the first share of the process, which is announced on
/// its own and explains that the lines to come are summaries - otherwise the
/// quiet that follows reads as a pool that stopped accepting work.
fn share_summary_line(
    count: u64,
    span_ms: Option<u64>,
    height: u64,
    workers: &BTreeSet<String>,
    capped: bool,
) -> String {
    let named: Vec<&str> = workers
        .iter()
        .take(SHARE_LOG_NAMES)
        .map(|w| w.as_str())
        .collect();
    let mut who = named.join(", ");
    let unnamed = workers.len().saturating_sub(named.len());
    if unnamed > 0 {
        who.push_str(&format!(" (+{unnamed} more)"));
    }
    let Some(span) = span_ms else {
        return format!(
            "[shares] first accepted share, height {height}, from {who}. From here the pool \
             prints one summary line every {}s instead of one line per share.",
            SHARE_LOG_EVERY_MS / 1_000
        );
    };
    let n = workers.len();
    let at_least = if capped { "at least " } else { "" };
    let plural = if n == 1 { "" } else { "s" };
    format!(
        "[shares] {count} accepted in the {}s since the last line, height {height}, from \
         {at_least}{n} worker{plural}: {who}",
        span / 1_000
    )
}

/// The line a found block gets, and it is NEVER rate limited.
///
/// This used to be the same println as the per-share line, which is how a
/// thousand share lines a second could hide the only event that earns the pool
/// anything. Printed before the block is submitted, so that even a submission
/// that hangs or is refused leaves the operator knowing a block was found here
/// and which worker found it.
fn block_found_line(worker: &str, height: u64, hash: &[u8; 32]) -> String {
    format!(
        "[block] SOLVED height {height} by {worker}, hash {}; submitting it to the node now",
        hex::encode(hash)
    )
}

/// One worker's submission budget, in shares.
///
/// Refilled at the rate the share target says a worker COULD find shares, so a
/// burst of shares that were found over minutes and held back cannot all be
/// inserted at once. That burst is the whole withholding attack: the payout
/// window is only PPLNS_WINDOW deep, and a miner that can fill all of it in a
/// second owns every payout it chooses to.
#[derive(Debug, Clone, Copy)]
struct ShareRate {
    shares: u64,
    at_ms: u64,
}

/// Shares per second one worker id may sustain, from what one share COSTS.
///
/// `share_cost_bits` is how many hashes a share is worth as a power of two, so
/// this is simply the ceiling hashrate divided by the price of a share. Never
/// zero: a worker must always be able to keep submitting, or a chain with very
/// expensive shares would lock every miner out of its own earnings.
fn worker_share_rate(share_cost_bits: u32) -> u64 {
    let shift = MAX_WORKER_HASHRATE_LOG2
        .saturating_sub(share_cost_bits)
        .min(40);
    1u64 << shift
}

/// How many shares a worker may hold in hand at once.
fn worker_burst(per_sec: u64) -> u64 {
    per_sec
        .saturating_mul(WORKER_BURST_SECS)
        .max(WORKER_BURST_MIN_SHARES)
}

/// Spend one share from `st`'s budget, refilling it for the time since it was
/// last used. False means this worker is submitting faster than any hardware on
/// this chain could find shares.
fn rate_admits(st: &mut ShareRate, now_ms: u64, per_sec: u64, burst: u64) -> bool {
    let dt = now_ms.saturating_sub(st.at_ms);
    st.at_ms = now_ms;
    let refill = per_sec.saturating_mul(dt) / 1_000;
    st.shares = st.shares.saturating_add(refill).min(burst);
    if st.shares == 0 {
        return false;
    }
    st.shares -= 1;
    true
}

impl Pool {
    /// Snapshot accounting for the disk at most every 16 shares. Block events
    /// snapshot directly, so a crash loses at worst a handful of shares. The
    /// caller must RELEASE the pool lock before handing the result to
    /// `flush_state`: this is the share hot path, and every other request is
    /// serialized behind this same mutex.
    fn note_share_saved(&mut self) -> Option<StateShot> {
        self.unsaved += 1;
        if self.unsaved < 16 {
            return None;
        }
        self.unsaved = 0;
        // Non-durable: high frequency, and losing <=16 shares on a crash is
        // already the accepted tolerance. Block/settle events fsync.
        self.state_shot(false)
    }

    /// Derive the share target from the CURRENT network difficulty, so it tracks
    /// difficulty changes instead of being a fixed absolute threshold, and
    /// re-derive the factor it ACTUALLY achieves: the derivation saturates, and a
    /// difficulty fall can quietly turn a 2^24 share into "every hash counts".
    ///
    /// The verdict is re-derived here too, by the SAME function startup uses. It
    /// used to be a startup-only check with a ratio-only echo on the running path,
    /// and that left the entire band of network difficulty from 40 bits down to 18
    /// passing in silence - including the region where the target saturates and
    /// every hash is a share. A pool in that state keeps crediting and keeps
    /// paying, and the money has left the wallet by the time anyone reads a log.
    fn recompute_share_target(&mut self) {
        self.share_target = pool_core::share_target_hash(self.tpl.difficulty, self.share_factor);
        self.share_factor_achieved =
            pool_core::achieved_share_factor(&self.network_target, &self.share_target);
        self.share_cost_bits = pool_core::share_cost_bits(&self.share_target);
        self.share_halt = check_share_target(
            self.share_factor,
            self.share_factor_achieved,
            self.share_cost_bits,
            self.tpl.difficulty,
        )
        .err();
    }

    /// Rebuild the derived in-flight total from the payout rows.
    fn rebuild_inflight(&mut self) {
        self.inflight_units = self
            .payout_records
            .iter()
            .map(|r| r.units())
            .fold(0u64, |a, b| a.saturating_add(b));
    }

    /// Everything `/earnings` needs about one worker, gathered in one short hold
    /// of the pool lock. No node call, no full share table, no allocation beyond
    /// the handful of in-flight rows that actually name this worker.
    fn earnings_of(&self, worker: &str, now_ms: u64) -> Earnings {
        let shares = self.pplns.count_of(worker);
        // What a settlement would actually split over, read at ONE instant for
        // both halves of the ratio. Quoting a pending figure off the headcount
        // while the settlement splits credit would show every miner a number the
        // pool has no intention of paying.
        let (credit, window_credit) = self.pplns.credit_share(worker, now_ms);
        let paid = self.paid.get(worker).cloned().unwrap_or_default();
        let mut inflight = Vec::new();
        let mut inflight_units = 0u64;
        for r in &self.payout_records {
            let u = r.units_for(worker);
            if u == 0 {
                continue;
            }
            inflight_units = inflight_units.saturating_add(u);
            inflight.push(InflightRow {
                hash: r.hash.clone(),
                units: u,
                at: r.at,
                node_holds: r.node_holds,
            });
        }
        // Money a failed chunk still owes this worker. It is NOT an estimate and
        // not a share of anything: it is a row the pool already computed for this
        // address and could not deliver, and the next settlement pays it first.
        let mut owed_units = 0u64;
        let mut owed_total = 0u64;
        for (w, u) in &self.owed {
            owed_total = owed_total.saturating_add(*u);
            if w == worker {
                owed_units = owed_units.saturating_add(*u);
            }
        }
        Earnings {
            // The pool knows a worker if it holds shares for it now, has paid it,
            // owes it something in flight or from a failed chunk, or has handed
            // it work. Anything else is an address this pool has never heard of,
            // which is NOT the same fact as a worker that is owed nothing.
            known: shares > 0
                || credit > 0
                || paid.units > 0
                || inflight_units > 0
                || owed_units > 0
                || self.workers.contains_key(worker),
            shares,
            credit,
            window_credit,
            window_shares: self.pplns.total(),
            window_size: self.pplns.window() as u64,
            paid,
            paid_since: self.paid.since,
            inflight_units,
            inflight,
            owed_units,
            // The confirmed balance still holds money that is already inside a
            // submitted payout, and money the next settlement owes a named miner
            // before it splits anything. The pool-wide pending pot is therefore
            // what has matured MINUS both. Without this the same units would be
            // reported to the same miner twice - and worse, promised to miners
            // they are not owed to.
            pool_pending_units: self.matured.map(|m| {
                m.units
                    .saturating_sub(self.inflight_units)
                    .saturating_sub(owed_total)
            }),
            matured_at: self.matured.map(|m| m.at).unwrap_or(0),
            matured_current: self.matured_current,
            // Payouts the pool is tracking but has no rows for: written by an
            // older build, or by a tool that recorded only the hash. Their units
            // are still inside the confirmed balance, so the subtraction above
            // cannot be complete and PENDING would be overstated. Counted here so
            // it can be reported as unknown rather than as a number that is too
            // high, which is the one direction that would promise money.
            unattributed_payouts: self
                .settle_pending_txs
                .iter()
                .filter(|h| !self.payout_records.iter().any(|r| &&r.hash == h))
                .count(),
        }
    }

    /// Rebuild the cached standard-API pending response for the current template.
    fn rebuild_pending_cache(&mut self) {
        self.pending_cache = pending_cache_json(&self.tpl, &self.share_target);
    }

    /// Record one above-target submission. Returns the line to print, or `None`
    /// when this streak has already been reported recently enough.
    fn note_bad_share(&mut self, worker: &str, now_ms: u64) -> Option<String> {
        let since = now_ms.saturating_sub(self.tpl_changed_at_ms);
        let streak = bump_bad_streak(&mut self.bad_streak, worker, now_ms)?;
        Some(bad_streak_message(worker, streak, since))
    }

    /// A share was accepted: this worker and the pool agree on the header again.
    fn note_good_share(&mut self, worker: &str) {
        self.bad_streak.remove(worker);
    }

    /// May this worker insert one more share right now?
    ///
    /// False means it is submitting faster than the share target says any
    /// hardware on this chain could FIND shares, which is what a batch of
    /// withheld shares looks like on the wire.
    fn rate_admits_share(&mut self, worker: &str, now_ms: u64) -> bool {
        let per_sec = worker_share_rate(pool_core::share_cost_bits(&self.share_target));
        let burst = worker_burst(per_sec);
        if !self.rates.contains_key(worker) && self.rates.len() >= RATE_WORKERS {
            // Drop the entries a refill would have filled to the brim anyway:
            // forgetting those changes nothing about what they may submit.
            let idle = WORKER_BURST_SECS.saturating_mul(1_000);
            self.rates
                .retain(|_, st| now_ms.saturating_sub(st.at_ms) < idle);
            if self.rates.len() >= RATE_WORKERS {
                // Fail OPEN. Refusing an honest miner's work because a bookkeeping
                // map is full costs it real money; the residence weighting is what
                // actually decides the split, and it does not depend on this.
                return true;
            }
        }
        let st = self.rates.entry(worker.to_string()).or_insert(ShareRate {
            shares: burst,
            at_ms: now_ms,
        });
        rate_admits(st, now_ms, per_sec, burst)
    }

    /// Stable per-worker extranonce -> private search space (coinbase miner_nonce).
    /// The /work protocol is anonymous, so cap the map: past the cap, hand out a
    /// deterministic extranonce derived from the name instead of storing it, so a
    /// flood of unique names cannot grow memory without bound.
    fn extranonce_for(&mut self, worker: &str) -> [u8; 32] {
        if let Some(en) = self.workers.get(worker) {
            return *en;
        }
        if self.workers.len() >= 100_000 {
            let mut en = [0u8; 32];
            en[0..8].copy_from_slice(&(worker.len() as u64).to_be_bytes());
            for (i, b) in worker.bytes().enumerate() {
                en[8 + (i % 24)] ^= b;
            }
            return en;
        }
        self.next_en += 1;
        let mut en = [0u8; 32];
        en[24..32].copy_from_slice(&self.next_en.to_be_bytes());
        self.workers.insert(worker.to_string(), en);
        en
    }

    /// Serialize the accounting for `flush_state` to write once the pool lock is
    /// released. `durable` fsyncs before the rename (block-found and settlement
    /// events); the debounced share-save does not.
    fn state_shot(&mut self, durable: bool) -> Option<StateShot> {
        if self.state_file.is_empty() {
            return None;
        }
        let body = json!({
            "window": PPLNS_WINDOW,
            // (worker, arrival time in ms). The times are not decoration: without
            // them a restart would reset every share's age to zero and hand the
            // next settlement to whoever submits first afterwards.
            "order": self.pplns.snapshot(),
            // Credit already earned by shares that have left the window. Losing
            // it on a restart would silently cut the payout of every miner whose
            // shares had rolled through.
            "banked": self.pplns.banked_snapshot().into_iter().map(|(at, rows)| json!({
                "at": at,
                "rows": rows,
            })).collect::<Vec<_>>(),
            // So hbit-pool-payout weighs work exactly as this server does when it
            // settles from this file with the server stopped.
            "credit_horizon_ms": self.pplns.horizon_ms(),
            "accepted": self.accepted,
            "blocks": self.blocks,
            "orphaned": self.orphaned,
            "settle_pending_txs": self.settle_pending_txs,
            // The per-recipient rows behind those hashes, and the confirmed
            // totals they turn into. Both must move in the SAME snapshot: a
            // payout leaves the in-flight rows at the instant it enters the paid
            // totals, and a crash between the two would either lose a payment or
            // report it twice.
            "payouts_inflight": self.payout_records.iter().map(|r| r.to_json())
                .collect::<Vec<_>>(),
            // What a failed chunk still owes named miners. Losing this on a
            // restart means those miners are never paid for that window at all:
            // the money quietly rejoins the pot and is split over whoever is
            // mining next.
            "owed": owed_to_json(&self.owed),
            "paid": self.paid.to_json(),
            // Blocks still awaiting confirmation. Without these a restart in the
            // window between finding a block and burying it drops it from the
            // confirm/orphan reconciliation for good, so a later reorg of one of
            // OUR blocks is never detected and the operator's stats drift.
            "submitted": self.submitted.iter().map(|(h, hash)| json!({
                "height": h,
                "hash": hex::encode(hash),
            })).collect::<Vec<_>>(),
            "immature": self.immature.iter().map(|e| json!({
                "height": e.height,
                "hash": hex::encode(e.hash),
                "units": e.units,
                // Without this a restart would read `units` as the block's WHOLE
                // income and never add the transaction fees the chain credited
                // alongside it, which is money paid out at zero confirmations.
                "fees_counted": e.fees_counted,
            })).collect::<Vec<_>>(),
            // The header timestamp currently being served, so a restart inside a
            // height reproduces the SAME 89 bytes instead of re-stamping them.
            // Without it a restart silently invalidates every worker's in-flight
            // scan pass: measured on a rig, a restart at height 350 served a
            // stamp 68 seconds later than the one already in flight, and
            // /query/miner/notice only signals a HEIGHT change so nothing told
            // the workers to reload. Thousands of shares were hashed into
            // nothing before their scan passes ended.
            "template_stamp": {
                "height": self.tpl.height,
                // The parent as well as the height: after a same-height reorg the
                // old stamp belongs to a block on a different parent, and reusing
                // it would put a timestamp in the header that was never checked
                // against the parent this block actually follows.
                "prevhash": self.tpl.prevhash.to_hex(),
                "timestamp": self.tpl.timestamp,
            },
        });
        self.state_seq += 1;
        Some(StateShot {
            seq: self.state_seq,
            path: self.state_file.clone(),
            body: body.to_string().into_bytes(),
            durable,
        })
    }

    fn load_state(&mut self) {
        let Ok(txt) = std::fs::read_to_string(&self.state_file) else {
            return;
        };
        let j: serde_json::Value = match serde_json::from_str(&txt) {
            Ok(j) => j,
            Err(e) => {
                // Never silently wipe accounting: preserve the corrupt file and
                // start fresh only after loudly flagging it for the operator.
                let bak = format!("{}.corrupt.{}", self.state_file, std::process::id());
                let _ = std::fs::rename(&self.state_file, &bak);
                eprintln!(
                    "[state] file corrupt ({e}); preserved as {bak}, starting with empty accounting"
                );
                return;
            }
        };
        // A file written before shares were timed carries bare worker ids. Every
        // one of them is given the same arrival time, one horizon back, so the
        // window comes back weighing exactly what the older build weighed it at:
        // a restart must not move money between miners, in either direction.
        let horizon = self.pplns.horizon_ms();
        let order = parse_share_order(&j, pool_core::now_ms().saturating_sub(horizon));
        let banked = parse_banked_credit(&j);
        self.pplns = Pplns::restore(PPLNS_WINDOW, horizon, order, banked);
        self.accepted = j.get("accepted").and_then(|v| v.as_u64()).unwrap_or(0);
        self.blocks = j.get("blocks").and_then(|v| v.as_u64()).unwrap_or(0);
        self.orphaned = j.get("orphaned").and_then(|v| v.as_u64()).unwrap_or(0);
        self.settle_pending_txs = j
            .get("settle_pending_txs")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        // Blocks awaiting confirmation must survive a restart too, or one of OUR
        // blocks being orphaned goes unnoticed and blocks_confirmed permanently
        // over-counts against what the chain actually holds.
        self.submitted = j
            .get("submitted")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        let h = x.get("height").and_then(|v| v.as_u64())?;
                        let hash = hash32(x.get("hash").and_then(|v| v.as_str())?)?;
                        Some((h, hash))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The hold-back must survive a restart: forgetting it would let the very
        // next settle cycle distribute income a reorg can still revoke.
        self.immature = j
            .get("immature")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        let height = x.get("height").and_then(|v| v.as_u64())?;
                        let units = x.get("units").and_then(|v| v.as_u64())?;
                        let hash = hash32(x.get("hash").and_then(|v| v.as_str())?)?;
                        // A file written by a build that held back the subsidy
                        // alone has no flag, and its entries really do carry the
                        // subsidy alone. Reading a missing flag as "counted"
                        // would distribute those blocks' transaction fees on the
                        // first settlement after the upgrade.
                        let fees_counted = x
                            .get("fees_counted")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        Some(Immature {
                            height,
                            hash,
                            units,
                            fees_counted,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The per-worker settlement ledger. Losing this would not lose anyone's
        // money - the chain holds that - but it WOULD reset every miner's "total
        // paid" to zero and make the pool report a number that quietly means
        // something else, so it is restored with everything else and its start
        // time travels with it.
        self.payout_records = parse_payout_records(&j);
        self.owed = parse_owed(&j);
        self.paid = parse_paid_ledger(&j);
        self.rebuild_inflight();
        let owed_units: u64 = self
            .owed
            .iter()
            .map(|(_, u)| *u)
            .fold(0u64, |a, b| a.saturating_add(b));
        println!(
            "restored accounting: {} shares in window, {} blocks, {} orphaned, \
             {} block(s) awaiting confirmation, {} payout(s) pending, \
             {} block(s) of income not yet matured, \
             {} worker(s) with a paid history, \
             {owed_units} unit(s) owed to {} worker(s) by a settlement that failed",
            self.pplns.total(),
            self.blocks,
            self.orphaned,
            self.submitted.len(),
            self.settle_pending_txs.len(),
            self.immature.len(),
            self.paid.workers(),
            self.owed.len()
        );
    }
}

/// The header timestamp a previous run of this pool was serving, read back out of
/// a state-file body so a restart inside a height reproduces the same header.
///
/// Any doubt at all yields `None`, and the pool then stamps a fresh template
/// exactly as it always did. The two outcomes are not symmetric: a MISSING pin
/// costs one height's worth of in-flight worker scan passes and self-heals, while
/// a WRONG pin puts a timestamp into a block the node may then refuse - and
/// `/submit/block` validates asynchronously, so a refused block is silent and its
/// entire reward is gone. `hbit_pool::template_timestamp` re-checks the pin
/// against the live parent for the same reason; this is the cheap first filter.
fn parse_stamp_pin(j: &serde_json::Value) -> Option<StampPin> {
    let s = j.get("template_stamp")?;
    let height = s.get("height").and_then(|v| v.as_u64())?;
    let timestamp = s.get("timestamp").and_then(|v| v.as_u64())?;
    let prevhash = Hash::from_hex(s.get("prevhash").and_then(|v| v.as_str())?.as_bytes()).ok()?;
    Some(StampPin {
        height,
        prevhash,
        timestamp,
    })
}

/// `parse_stamp_pin` against the state file at `path`, if there is one.
///
/// A file that is absent, unreadable or unparseable is not reported here: this
/// runs before `load_state`, which is what owns the corrupt-state-file message,
/// and printing it twice would have an operator chasing two faults.
fn read_stamp_pin(path: &str) -> Option<StampPin> {
    let txt = std::fs::read_to_string(path).ok()?;
    let j: serde_json::Value = serde_json::from_str(&txt).ok()?;
    parse_stamp_pin(&j)
}

/// Count one above-target submission from `worker`, returning the streak length
/// when it is time to say so.
///
/// Shouts once the streak reaches `BAD_STREAK_WARN`, then at most once every
/// `BAD_STREAK_REPEAT_MS` for as long as it lasts, so a worker that never
/// recovers keeps saying so without burying every other line in the log. The map
/// is bounded: a flood of invented worker ids must not grow memory, and the
/// diagnostic exists for real miners.
///
/// The old rule fired at every multiple of `BAD_STREAK_WARN`, which is a fixed
/// tax on the reject RATE rather than on elapsed time: a fast worker hashing a
/// header the pool had just re-stamped produced hundreds of identical lines in a
/// few minutes.
fn bump_bad_streak(
    streaks: &mut HashMap<String, BadStreak>,
    worker: &str,
    now_ms: u64,
) -> Option<u64> {
    if !streaks.contains_key(worker) && streaks.len() >= BAD_STREAK_WORKERS {
        return None;
    }
    let st = streaks.entry(worker.to_string()).or_default();
    st.count += 1;
    if st.count < BAD_STREAK_WARN {
        return None;
    }
    if let Some(at) = st.warned_at_ms {
        if now_ms.saturating_sub(at) < BAD_STREAK_REPEAT_MS {
            return None;
        }
    }
    st.warned_at_ms = Some(now_ms);
    Some(st.count)
}

/// The line printed for a run of above-target submissions.
///
/// Reports only what the pool OBSERVED, never a verdict on the worker. The pool
/// cannot tell the two causes apart: whether a worker computes the merkle root
/// wrongly, or is simply still finishing a scan pass on a header the pool has
/// since replaced, the only evidence either way is that the header it hashed is
/// not the header the pool rebuilds. Both look identical from here.
///
/// The previous wording stated a permanent worker-side fault as fact - "Nothing
/// this worker submits can be credited until that is fixed". A pool restart
/// re-stamps the template mid-height and changes the header under every connected
/// worker, so on a rig that line was printed 1,281 times for a transient
/// condition the POOL had caused, about workers that were behaving correctly and
/// recovered on their own. An operator who believes it goes and rebuilds a worker
/// that was never wrong, while whatever is really happening goes unexamined -
/// which is worse than saying nothing at all.
fn bad_streak_message(worker: &str, streak: u64, since_change_ms: u64) -> String {
    let secs = since_change_ms / 1_000;
    let mut msg = format!(
        "[{worker}] {streak} shares in a row hashed above the share target; the pool last \
         changed the header it serves {secs}s ago. Observed, not diagnosed: the pool rebuilds \
         every share's header itself from (height, coinbase nonce, block nonce), and these \
         shares were built on a header it does not hold."
    );
    if since_change_ms < TEMPLATE_SETTLE_MS {
        msg.push_str(
            " The pool has just changed its template or just restarted, which changes the \
             header under every connected worker, and /query/miner/notice signals only a HEIGHT \
             change - so a worker keeps hashing the header it was handed until its current scan \
             pass ends. That alone explains this, it costs only the work already in flight, and \
             it clears by itself. Nothing to do yet.",
        );
    } else {
        msg.push_str(
            " That is well past the pool's last template change, so a worker still on a stale \
             header no longer accounts for it. Worth checking next: a worker that ignores \
             `mkrl_modify_list` from /query/miner/pending builds a different merkle root and \
             stays in this state. Shares are earning nothing for as long as it lasts.",
        );
    }
    msg
}

/// The standard-API `/query/miner/pending` body for a template.
///
/// `mkrl_modify_list` is NOT optional once the template carries the node's
/// transactions. A worker rebuilds the merkle root from its OWN coinbase nonce
/// folded through this list; serving an empty list while the block holds
/// transactions makes the worker hash a header the pool never reconstructs, so
/// every share it finds is rejected. The pool used to hard-code `[]` here, which
/// was safe only because its blocks were always coinbase-only.
fn pending_cache_json(tpl: &Template, share_target: &[u8; 32]) -> String {
    let cb = coinbase_with_extranonce(tpl, &[0u8; 32]);
    let intro = intro_bytes(tpl, &cb, 0);
    let mkrl: Vec<String> = tpl
        .txs
        .mrklrts
        .iter()
        .map(|h| hex::encode(h.serialize()))
        .collect();
    json!({
        "ret": 0,
        "height": tpl.height,
        "block_intro": hex::encode(intro),
        "target_hash": hex::encode(share_target),
        "coinbase_body": coinbase_body_hex(&cb),
        "mkrl_modify_list": mkrl,
    })
    .to_string()
}

/// Refuse a share size that would make the equal-weight PPLNS window unfair.
///
/// Every accepted share is credited with weight 1, and the network difficulty in
/// force when it was mined is not recorded. That is exact only while the whole
/// window is far SHORTER than one block interval, because difficulty moves only
/// at a block boundary: with 2^factor shares to a block, PPLNS_WINDOW shares
/// span PPLNS_WINDOW / 2^factor of a block - 0.02% at the default 24, still only
/// 1.6% at 18. Go lower and a difficulty change lands inside a live window, so
/// real payout money is split by share counts that stand for different amounts
/// of work. The upper bound keeps a share from being so easy that a whole GPU
/// batch always beats it (credit would then track batch cadence, not hashrate)
/// and that the share target degenerates into the all-0xff ceiling.
fn check_share_factor(factor: u32) -> Result<(), String> {
    if !(MIN_SHARE_FACTOR..=MAX_SHARE_FACTOR).contains(&factor) {
        return Err(format!(
            "<share_bits> must be between {MIN_SHARE_FACTOR} and {MAX_SHARE_FACTOR} (got {factor}).\n\
             A share is 2^share_bits easier than a network block; below {MIN_SHARE_FACTOR} the \
             {PPLNS_WINDOW}-share payout window covers enough of a block interval that a \
             difficulty change inside it would misallocate real payouts, and above \
             {MAX_SHARE_FACTOR} a share costs so little that credit stops tracking hashrate.\n\
             What to do: pass {DEFAULT_SHARE_BITS} as argument 4 unless you have measured \
             otherwise."
        ));
    }
    Ok(())
}

/// Refuse a share target the live network difficulty cannot support.
///
/// `check_share_factor` only ever looked at the number the operator typed. The
/// target actually served is `network_target * 2^factor`, computed by
/// `share_target_hash`, which SATURATES at the all-0xff ceiling and says nothing
/// when it does. On a low-difficulty chain - a fresh testnet sits at
/// `LOWEST_DIFFICULTY`, whose target has no leading zero bits at all - every hash
/// then beats the share target. A worker is no longer credited for work: it is
/// credited for how often it can complete an HTTP round trip, and one worker in a
/// tight loop takes the whole payout window from everyone else. That is not a
/// tuning wrinkle, it is the pool paying the wrong people, so it is refused.
///
/// TWO bounds are applied, because a ratio alone does not make a share honest.
///
/// `achieved` is a ratio: how much easier a share is than a block. `cost_bits` is
/// what the share itself costs, as a power of two hashes. The first bound was
/// here alone, and it has a hole exactly wide enough to lose money through: on a
/// chain whose target has 22 leading zero bits, `share_bits` 24 saturates to the
/// all-0xff ceiling, `achieved` reads 22, clears `MIN_SHARE_FACTOR`, and every
/// hash beats the share target anyway. That is the very condition this function
/// was written to refuse, passing its own check.
///
/// `leading_zero_bits(network) == achieved + cost_bits`, so lowering `share_bits`
/// moves work from the ratio into the cost, down to `MIN_SHARE_FACTOR`. Below
/// that the chain is too easy at any setting, and the two cases need different
/// advice, so they are reported differently.
fn check_share_target(
    factor: u32,
    achieved: u32,
    cost_bits: u32,
    difficulty: u32,
) -> Result<(), String> {
    if achieved < MIN_SHARE_FACTOR {
        return Err(format!(
            "the network difficulty in force ({difficulty}) is too low to serve a fair share.\n\
             share_bits={factor} asks for a share 2^{factor} easier than a block, but the \
             derived share target saturates and what workers actually get is 2^{achieved} \
             (minimum {MIN_SHARE_FACTOR}).\n\
             At that point a share costs almost no work, so PPLNS credit tracks how fast a \
             worker can submit rather than how much it hashes, and one worker can take the \
             whole {PPLNS_WINDOW}-share payout window from everyone else. This pool will not \
             distribute real money on that basis.\n\
             What to do: point the pool at a chain whose difficulty has risen (mainnet is far \
             above this), or wait for this one to adjust upward and start it again. Lowering \
             <share_bits> does not help: the ceiling is the chain's, not yours."
        ));
    }
    if cost_bits < MIN_SHARE_COST_BITS {
        // network_bits is what the chain offers in total; the best a legal
        // share_bits can leave for the cost is network_bits - MIN_SHARE_FACTOR.
        let network_bits = achieved + cost_bits;
        let best_cost = network_bits.saturating_sub(MIN_SHARE_FACTOR);
        let advice = if best_cost >= MIN_SHARE_COST_BITS {
            let highest = network_bits.saturating_sub(MIN_SHARE_COST_BITS);
            format!(
                "What to do: lower <share_bits> to {highest} or less. The chain offers \
                 2^{network_bits} of work per block and share_bits spends {factor} of it on \
                 making shares frequent, leaving 2^{cost_bits} for the share itself. Spending \
                 less leaves more."
            )
        } else {
            format!(
                "What to do: nothing here helps. The chain offers only 2^{network_bits} of work \
                 per block, and share_bits cannot go below {MIN_SHARE_FACTOR}, so the most a \
                 share could ever cost on it is 2^{best_cost}. Point the pool at a chain whose \
                 difficulty has risen; mainnet is far above this."
            )
        };
        return Err(format!(
            "the network difficulty in force ({difficulty}) is too low to serve a share worth \
             counting.\n\
             share_bits={factor} leaves each share costing 2^{cost_bits} hashes on average \
             (minimum 2^{MIN_SHARE_COST_BITS}). The ratio looks healthy - a share is 2^{achieved} \
             easier than a block - but a ratio only means something once the share itself costs \
             work.\n\
             At 2^{cost_bits} a worker is credited for completing an HTTP round trip rather than \
             for hashing, so the fastest submitter takes the {PPLNS_WINDOW}-share window from \
             miners doing more work. This pool will not distribute real money on that basis.\n\
             {advice}"
        ));
    }
    Ok(())
}

/// Plan a settlement on the pool's ADVERTISED terms.
///
/// The only place the split is parameterised, and `/terms` reports the very same
/// constants, so the fee and the minimum payout a miner is told about are the
/// fee and the minimum payout the pool applies.
fn plan_settlement(distributable: u64, payable: &[(String, u64)]) -> Vec<(String, u64)> {
    split_payout(distributable, POOL_FEE_UNITS, PAYOUT_DUST_UNITS, payable)
}

/// The per-worker figures a settlement divides money by: PPLNS CREDIT, never the
/// raw share headcount.
///
/// The headcount is the composition of the window at one instant, and the pool
/// publishes both halves of when that instant falls - `settle_interval_secs` and
/// `window_shares` in `/terms`. A miner can therefore hold its shares back for a
/// whole interval, dump a full window's worth in the second before a settlement
/// runs, evict every honest share, and be the only name in the split. Credit
/// weighs a share by how long it has stood in the window, so a dump is worth
/// nothing at the moment it lands and the shares it evicted keep what they had
/// already earned.
fn settlement_credit(p: &Pool, now_ms: u64) -> Vec<(String, u64)> {
    p.pplns.credit(now_ms)
}

/// A money figure, in the chain's own terms.
///
/// `units` is a whole number of 0.1 HAC (the granularity every payout this pool
/// makes is planned in) and `amount` is that same number as the chain's `Amount`
/// in its canonical `mantissa:unit` form - the identical string the node speaks
/// and the transaction carries. No float, no hand-rolled decimal.
fn money(units: u64) -> serde_json::Value {
    json!({
        "units": units,
        "amount": payout_amount(units).to_fin_string(),
        "unit": PAYOUT_UNIT,
    })
}

/// What `/earnings` can say about a worker id before it looks anything up.
///
/// Three different facts, and a miner must never be shown one when another is
/// true: a string that is not an address at all, an address this pool could never
/// pay (so it never credits shares to it), and an address that is fine - after
/// which "the pool has never heard of it" and "it is owed nothing" are two more
/// answers again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerId {
    Payable,
    Unpayable,
    NotAnAddress,
}

fn classify_worker_id(s: &str) -> WorkerId {
    if Address::from_readable(s).is_err() {
        return WorkerId::NotAnAddress;
    }
    if is_payout_address(s) {
        WorkerId::Payable
    } else {
        WorkerId::Unpayable
    }
}

/// One payout that is submitted but not yet confirmed, as it concerns one worker.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InflightRow {
    hash: String,
    units: u64,
    at: u64,
    node_holds: bool,
}

/// Everything `/earnings` says about one worker, gathered under the pool lock and
/// rendered with it released.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Earnings {
    /// Has this pool ever heard of this address? A worker it has never seen is a
    /// different fact from a worker that is owed nothing, and reporting the first
    /// as the second tells a miner its pool is tracking work that it is not.
    known: bool,
    shares: u64,
    /// What a settlement would weigh this worker by: how long its shares have
    /// been in the window, in milliseconds. NOT a headcount - see `Pplns`.
    credit: u64,
    /// The same figure summed over every worker, read at the same instant.
    window_credit: u64,
    window_shares: u64,
    window_size: u64,
    paid: hbit_pool::PaidRow,
    paid_since: u64,
    inflight_units: u64,
    inflight: Vec<InflightRow>,
    /// Money a settlement chunk was going to pay this worker and did not. Already
    /// computed for this address, not a share of a pot, and paid before anything
    /// else next cycle.
    owed_units: u64,
    /// Pool-wide matured-and-unsettled pot, already net of everything in flight
    /// and everything owed. `None` means the pool cannot value its own wallet
    /// right now.
    pool_pending_units: Option<u64>,
    matured_at: u64,
    matured_current: bool,
    /// Tracked payouts whose per-recipient rows this pool does not have. While
    /// there is even one, the pending pot cannot be computed honestly.
    unattributed_payouts: usize,
}

/// One worker's share of the pool-wide pending pot.
///
/// The same floor split `split_payout` applies, for one worker: a settlement
/// hands out `pot * mine / total` rounded down, and anything under the minimum
/// payout is not paid at all. Deliberately does NOT add the largest-remainder
/// unit `split_payout` may award, so this figure is never higher than what a
/// settlement at this instant would actually pay.
///
/// Every share in the window belongs to a payable address - `handle_submission`
/// refuses any other - so the window total IS the payable total the split runs
/// over.
///
/// `mine` and `window_total` are CREDIT, the same figures a settlement splits on;
/// quoting the estimate off share counts while the settlement pays credit would
/// promise a miner money the pool has no intention of sending.
fn worker_pending_units(pot: u64, mine: u64, window_total: u64) -> u64 {
    if pot == 0 || mine == 0 || window_total == 0 {
        return 0;
    }
    let share = (pot as u128 * mine as u128 / window_total as u128) as u64;
    // Below the advertised minimum a settlement pays nothing at all, so promising
    // a fraction of it would be promising money that never moves.
    if share < PAYOUT_DUST_UNITS { 0 } else { share }
}

/// The `/earnings` body for one worker.
fn earnings_json(worker: &str, e: &Earnings, now: u64) -> serde_json::Value {
    if !e.known {
        // No numbers at all. A zero here would read as "your pool owes you
        // nothing", when the truth is that it has never seen this address.
        return json!({
            "ok": true,
            "kind": "unknown_worker",
            "worker": worker,
            "known": false,
            "err": "this pool has no record of that address: it holds no shares for it, has \
                    never paid it, and has no payout in flight for it",
        });
    }
    let pending = match e.pool_pending_units.filter(|_| e.unattributed_payouts == 0) {
        Some(pot) => {
            let units = worker_pending_units(pot, e.credit, e.window_credit);
            json!({
                "known": true,
                "estimate": true,
                "units": units,
                "amount": payout_amount(units).to_fin_string(),
                "unit": PAYOUT_UNIT,
                "as_of_unix": e.matured_at,
                "as_of_age_secs": now.saturating_sub(e.matured_at),
                "current": e.matured_current,
                "note": "an ESTIMATE, not a promise: it is this worker's share of what the pool \
                         has matured and not yet settled, weighed by how long its shares have \
                         stood in the payout window. It moves every time any worker's share \
                         enters or leaves that window, and a share that has only just arrived \
                         has earned almost nothing yet.",
            })
        }
        // Rule of the house: never show a number the pool cannot stand behind.
        None if e.unattributed_payouts > 0 => json!({
            "known": false,
            "reason": format!(
                "{} payout(s) this pool is tracking have no recipient detail behind them, so it \
                 cannot separate what is still owed from what is already in flight and would \
                 overstate this figure. It resolves itself as those payouts confirm. This is not \
                 a zero.",
                e.unattributed_payouts
            ),
        }),
        None => json!({
            "known": false,
            "reason": "the pool has not been able to value its own wallet balance, so it cannot \
                       say what is owed. This is not a zero.",
        }),
    };
    json!({
        "ok": true,
        "kind": "worker",
        "worker": worker,
        "known": true,
        "scheme": "PPLNS",
        "shares_in_window": e.shares,
        "window_shares": e.window_shares,
        "window_size": e.window_size,
        // The numbers a settlement actually divides. Published so a miner can
        // check the split it was paid instead of taking the headcount above for
        // the basis of it.
        "credit_ms": e.credit,
        "window_credit_ms": e.window_credit,
        "credit_note": "credit is how long this worker's shares have been in the payout window, \
                        in milliseconds. Payouts are split by credit, not by the share count.",
        // Confirmed on chain. Only ever grows, and only when the node reports the
        // paying transaction buried.
        "paid": money(e.paid.units),
        "paid_since_unix": e.paid_since,
        "last_payout": if e.paid.last_units > 0 {
            json!({
                "units": e.paid.last_units,
                "amount": payout_amount(e.paid.last_units).to_fin_string(),
                "unit": PAYOUT_UNIT,
                "tx": e.paid.last_hash,
                "at_unix": e.paid.last_at,
            })
        } else {
            // Never paid by this ledger. An explicit null, not a zero amount.
            serde_json::Value::Null
        },
        // Submitted, not confirmed: neither paid nor pending.
        "in_flight": {
            "units": e.inflight_units,
            "amount": payout_amount(e.inflight_units).to_fin_string(),
            "unit": PAYOUT_UNIT,
            "txs": e.inflight.iter().map(|r| json!({
                "tx": r.hash,
                "units": r.units,
                "amount": payout_amount(r.units).to_fin_string(),
                "submitted_unix": r.at,
                // false = submitted, but the node's verdict could not be read.
                // The pool keeps tracking it and claims nothing about it.
                "node_holds": r.node_holds,
            })).collect::<Vec<_>>(),
            "note": "submitted to the node and NOT yet confirmed on chain. Nothing here is paid \
                     yet, and none of it is counted in pending.",
        },
        // A payout this pool planned for THIS address and could not deliver. It
        // is not an estimate and it does not depend on the share window: the next
        // settlement pays it before it splits anything else.
        "owed": {
            "units": e.owed_units,
            "amount": payout_amount(e.owed_units).to_fin_string(),
            "unit": PAYOUT_UNIT,
            "note": "already computed for this address by a settlement that did not reach the \
                     node. It is NOT a share of the pending pot and it does not expire with the \
                     share window: the next settlement pays it first.",
        },
        "pending": pending,
        "buckets": "paid, in_flight, owed and pending are disjoint: a unit is in exactly one of \
                    them.",
    })
}

/// The pool's terms, read out of the code that enforces them.
///
/// Every number here is the constant or the live value some other part of this
/// pool acts on, so what is advertised cannot drift from what is done. Nothing in
/// it is typed twice.
#[allow(clippy::too_many_arguments)]
fn terms_json(
    window_size: u64,
    credit_horizon_ms: u64,
    share_factor: u32,
    share_factor_achieved: u32,
    share_cost_bits: u32,
    share_halt: Option<&str>,
    difficulty: u32,
    settle_secs: u64,
    coinbase_maturity: u64,
    payout_confirm_depth: u64,
) -> serde_json::Value {
    json!({
        "ok": true,
        "scheme": "PPLNS",
        "scheme_full": "Pay Per Last N Shares",
        "scheme_note": "Every settlement is split over the last N accepted shares the pool holds \
                        at that moment, whoever found them. There are no rounds: this is NOT PROP, \
                        which would pay each block's shares in proportion to that block's round. A \
                        share's weight is how long it has been in that window, so work is paid for \
                        the time it stood in the window and not for the instant it arrived.",
        "window_shares": window_size,
        "credit_horizon_secs": credit_horizon_ms / 1_000,
        "credit_note": "a share is weighed by how long it has been in the payout window, capped \
                        at credit_horizon_secs, and what it earned before newer shares pushed it \
                        out is kept. A share that has only just arrived is worth almost nothing \
                        yet, so holding shares back and submitting them all at once earns LESS \
                        than sending them in as they are found - never more.",
        "share_factor": share_factor,
        "share_factor_achieved": share_factor_achieved,
        "share_factor_note": "a share is 2^share_factor_achieved times easier to find than a \
                              network block at the current difficulty. This is what the pool is \
                              REALLY serving, re-derived on every template change: the derivation \
                              saturates, so it can be far below the share_factor asked for.",
        "share_cost_bits": share_cost_bits,
        "share_cost_note": "what one share costs, as a power of two hashes. share_factor_achieved \
                            is only a ratio, and a ratio says nothing until the thing being \
                            divided costs work: a saturated target can read a healthy ratio while \
                            every hash on earth is a share. Both are checked on every template \
                            change, and a difficulty FALL can push either below its minimum while \
                            the pool is running.",
        "crediting_shares": share_halt.is_none(),
        "crediting_halt_reason": share_halt,
        "crediting_note": "while crediting_shares is false the pool credits no new shares and \
                           settles nothing, because credit would measure how fast a worker \
                           submits rather than how much it hashes and the money would go to the \
                           wrong miners. Shares already in the window keep what they have earned, \
                           and crediting resumes by itself if the difficulty recovers.",
        "network_difficulty": difficulty,
        "fee": money(POOL_FEE_UNITS),
        "fee_note": "this pool takes no fee: the whole matured balance is split over the share \
                     window.",
        "minimum_payout": money(PAYOUT_DUST_UNITS),
        "minimum_payout_note": "a worker whose share of a settlement rounds below this is paid \
                                nothing that cycle. That money is not taken from anyone: it stays \
                                in the pool wallet and is part of the next cycle's balance.",
        "fee_reserve": money(SETTLE_RESERVE_UNITS),
        "fee_reserve_note": "kept in the pool wallet so it can always fund the network fee on a \
                             settlement transaction. Not a fee, and not skimmed: a later cycle \
                             distributes whatever of it is no longer needed.",
        "network_fee_per_settlement_tx": chunk_tx_fee().to_fin_string(),
        "recipients_per_settlement_tx": PAYOUT_CHUNK,
        "settle_interval_secs": settle_secs,
        "coinbase_maturity_blocks": coinbase_maturity,
        "coinbase_maturity_note": "income from a block this pool finds is held back until the \
                                   chain has buried it this deep. Paying it out earlier and then \
                                   losing the block to a reorg would spend money the chain no \
                                   longer holds.",
        "payout_confirm_blocks": payout_confirm_depth,
        "payout_confirm_note": "a payout counts as PAID only once the node reports the paying \
                                transaction this many blocks deep. Until then it is in flight.",
        "share_eviction_note": "a share stops earning once newer shares push it out of the last \
                                window_shares the pool accepted. What it earned while it was in \
                                the window is kept for credit_horizon_secs and then expires.",
        "payout_address_required": true,
        "payout_address_note": "the worker id IS the payout address. The pool refuses a share \
                                from any id it could not pay, so nobody mines for an id that \
                                would be dropped at settlement.",
        "amount_unit": PAYOUT_UNIT,
        "amount_note": "`units` are whole 0.1 HAC and `amount` is the same figure as the chain's \
                        own mantissa:unit amount. The pool never reports money as a decimal it \
                        rounded itself.",
    })
}

/// A whole number of 0.1 HAC units written as HAC, for the operator-facing
/// startup text only.
///
/// One unit IS one tenth of a HAC, so a single decimal place is EXACT: this
/// rounds nothing and invents no precision. Money a miner is shown still goes
/// through `money()` and the chain's own `Amount`; this is for the two constants
/// the operator reads back on the console.
fn hac(units: u64) -> String {
    format!("{}.{} HAC", units / 10, units % 10)
}

/// A settlement interval the way an operator would say it out loud.
fn every(secs: u64) -> String {
    match secs {
        s if s >= 3600 && s % 3600 == 0 => format!("{}h", s / 3600),
        s if s >= 60 && s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// How a miner reaches this pool, worked out from what the pool is bound to, as
/// (the `connect =` value, the one thing about it the operator must know).
///
/// A wildcard bind carries no host anybody can dial and a loopback bind carries
/// one no OTHER machine can reach, so an operator who pastes the listen address
/// straight into a miner's config has sent that miner somewhere that will never
/// answer - and all the miner sees is a connection error with nothing to blame.
fn miner_connect(listen: &str) -> (String, &'static str) {
    let (host, port) = match listen.rsplit_once(':') {
        Some((h, p)) => (
            h.trim().trim_start_matches('[').trim_end_matches(']'),
            p.trim(),
        ),
        None => ("", listen.trim()),
    };
    match host {
        "0.0.0.0" | "::" | "*" | "" => (
            format!("<this machine's IP address or hostname>:{port}"),
            "bound to every interface: any machine that can reach this one can mine here",
        ),
        "127.0.0.1" | "localhost" | "::1" => (
            format!("{host}:{port}"),
            "loopback only: no other machine can mine here. Bind 0.0.0.0 when you are ready",
        ),
        _ => (
            format!("{host}:{port}"),
            "reachable from wherever that address is reachable",
        ),
    }
}

/// The terms this pool is about to enforce, in two operator-readable lines.
///
/// Every number here is the SAME constant `/terms` publishes and settlement
/// applies, so the console and the endpoint cannot tell a miner two different
/// stories. Nothing in it is typed twice.
fn terms_lines(window: u64, settle_secs: u64) -> [String; 2] {
    let fee = if POOL_FEE_UNITS == 0 {
        "no pool fee".to_string()
    } else {
        format!("pool fee {}", hac(POOL_FEE_UNITS))
    };
    [
        format!(
            "PPLNS over the last {window} shares, {fee}, minimum payout {}",
            hac(PAYOUT_DUST_UNITS)
        ),
        format!(
            "block income payable {COINBASE_MATURITY_DEPTH} blocks after this pool finds it, \
             settles every {}",
            every(settle_secs)
        ),
    ]
}

/// Everything needed to start this pool correctly, without reading a line of its
/// source. Printed on `--help` and on every refusal that comes from the command
/// line itself, because somebody who got one argument wrong is exactly the person
/// who cannot see what the other five should have been.
fn usage() -> String {
    format!(
        r"HBIT pool server v{ver}: serves work to miners, counts their shares, submits the
blocks it finds, and pays everybody out of a wallet it creates and holds.

usage:
  hbit-pool-server <node> <wallet_file> <listen> <share_bits> <chain> [settle_secs]

  <node>         Base URL of YOUR OWN Hacash fullnode, already running and synced.
                 The port is the `[server] listen` value in that node's
                 hacash.config.ini; the config this package ships uses 8080, so
                 the answer is normally http://127.0.0.1:8080

  <wallet_file>  Path to the pool's wallet key file, e.g. pool-wallet.key. It is
                 CREATED on the first run, and from that moment it holds the pool's
                 income and the money owed to your miners. There is no other copy.

  <listen>       <ip>:<port> to serve miners on. 0.0.0.0:9777 can be reached by any
                 machine that can reach this one, which is what a real pool wants.
                 127.0.0.1:9777 can be reached only from this machine, which is
                 what you want while you are still trying it out.

  <share_bits>   How many powers of two easier than a real block a share is,
                 {min_bits} to {max_bits}. Use {def_bits} unless you have measured otherwise.

  <chain>        Which chain your node is on. REQUIRED and never guessed, and
                 proved against the node itself before any work is served:
                   mainnet
                   testnet
                   testnet:<difficulty_adjust_blocks>:<each_block_target_time>

  [settle_secs]  Seconds between automatic payouts, {min_settle} to {max_settle}.
                 Default {def_settle} ({def_every}).

example, real Hacash, open to your other machines, paying out every {def_every}:
  hbit-pool-server http://127.0.0.1:8080 pool-wallet.key 0.0.0.0:9777 {def_bits} mainnet

Set a passphrase FIRST and the wallet file is written encrypted. Without one it is
a plaintext private key sitting on the disk:
  Windows PowerShell:  $env:{pw} = '<a long passphrase you have written down>'
  Linux / macOS:       export {pw}='<a long passphrase you have written down>'
Or put it in a file of its own and name that file in {pwf}.

A miner joins this pool with two settings in its poworker config:
  connect = <the address and port above>
  pool_worker = <that miner's own HAC address, which is where it gets paid>

The full runbook, including how to pay out by hand and what every refusal means,
is POOL-OPERATOR.md.",
        ver = env!("CARGO_PKG_VERSION"),
        min_bits = MIN_SHARE_FACTOR,
        max_bits = MAX_SHARE_FACTOR,
        def_bits = DEFAULT_SHARE_BITS,
        min_settle = MIN_SETTLE_SECS,
        max_settle = MAX_SETTLE_SECS,
        def_settle = DEFAULT_SETTLE_SECS,
        def_every = every(DEFAULT_SETTLE_SECS),
        pw = WALLET_PASSWORD_ENV,
        pwf = WALLET_PASSWORD_FILE_ENV,
    )
}

/// The one screen an operator reads back before letting real hashrate in: which
/// wallet the money will be paid FROM, which node the pool follows, the terms it
/// is about to enforce on other people's money, and the single line a miner needs.
#[allow(clippy::too_many_arguments)]
fn startup_summary(
    payout: &str,
    wallet_file: &str,
    encrypted: bool,
    node: &str,
    chain: &str,
    tip: Option<u64>,
    listen: &str,
    window: u64,
    settle_secs: u64,
    share_factor: u32,
    achieved: u32,
) -> String {
    let (connect, reach) = miner_connect(listen);
    let [terms_a, terms_b] = terms_lines(window, settle_secs);
    let tip = match tip {
        Some(h) => format!("at block {h}"),
        // Never claim a height that was not read: an unknown tip is unknown.
        None => "height unknown".to_string(),
    };
    // Above the refusal bound, so shares are still worth something, but not what
    // was asked for: say the real number rather than let the operator believe a
    // figure the chain in force will not support.
    let capped = if achieved < share_factor {
        format!(
            "\n               (share_bits={share_factor} was asked for; the difficulty in force \
             caps it at 2^{achieved})"
        )
    } else {
        String::new()
    };
    format!(
        r"----------------------------------------------------------------------
 HBIT pool is up. Read this back before you let anyone mine here.
   pays FROM   {payout}
               key file {wallet_file}, {prot}
   follows     {node} ({chain}, {tip})
   terms       {terms_a}
               {terms_b}
   share       2^{achieved} easier to find than a network block{capped}
   miners set  connect = {connect}
               pool_worker = <that miner's own HAC address>
               {reach}
   check it    http://{connect}/terms
----------------------------------------------------------------------",
        prot = if encrypted {
            "ENCRYPTED at rest"
        } else {
            "PLAINTEXT on disk, no passphrase set"
        },
    )
}

/// Said once, on the run that CREATES the wallet, as the last thing before the
/// pool starts serving: what this file is, and what losing it costs.
///
/// It is deliberately the final block on the screen. An operator who starts the
/// pool and walks away has to come back to this, not to a scrolled-off line.
fn new_wallet_banner(wallet_file: &str, payout: &str, encrypted: bool) -> String {
    let key_half = if encrypted {
        format!(
            "   The file is ENCRYPTED with the passphrase in {WALLET_PASSWORD_ENV}. THE TWO HALVES\n\
             \x20  ARE BOTH NEEDED: the file without the passphrase is noise, and the passphrase\n\
             \x20  without the file is a string of words. Back up BOTH, keep the passphrase\n\
             \x20  somewhere physical, and never rely on remembering it. There is no reset."
        )
    } else {
        format!(
            "   NO PASSPHRASE IS SET, so this file holds the private key in PLAINTEXT. Anything\n\
             \x20  that can read those bytes - a backup, a disk snapshot, an old drive, anyone\n\
             \x20  with the machine - can spend every coin the pool holds.\n\
             \x20  To fix it: stop the pool, set {WALLET_PASSWORD_ENV} to a passphrase of at least\n\
             \x20  {WALLET_PASSWORD_MIN} characters, and start it again. The file is then re-written encrypted."
        )
    };
    format!(
        r"**********************************************************************
 THIS RUN CREATED THE POOL'S WALLET. BACK IT UP NOW, BEFORE MINERS ARRIVE.
   file      {wallet_file}
   address   {payout}
 That file is the ONLY copy of the private key to that address. Every coin
 this pool mines lands there, including the money you will owe your miners.
 Lose the file and that money is gone permanently. Nobody can recover it:
 not the author of this software, not a support address, not the network.
   Copy it now to somewhere that survives this machine dying, and copy
   {state} with it: that is the record of who is owed what.
{key_half}
**********************************************************************",
        state = pool_state_path(wallet_file),
    )
}

/// Check the passphrase configuration BEFORE anything touches a key.
///
/// Mirrors `hbit_pool`'s own rule (the variable wins over the file; empty means
/// no passphrase at all) so that a passphrase which is too short, or a passphrase
/// file that cannot be read, is a refusal saying what to do instead of a panic
/// with a backtrace note on it - halfway through creating the very wallet the
/// passphrase was supposed to protect.
fn check_wallet_passphrase() -> Result<(), String> {
    let direct = Zeroizing::new(std::env::var(WALLET_PASSWORD_ENV).unwrap_or_default());
    if !direct.is_empty() {
        if direct.len() < WALLET_PASSWORD_MIN {
            return Err(format!(
                "the wallet passphrase in {WALLET_PASSWORD_ENV} is {} character(s); it must be at \
                 least {WALLET_PASSWORD_MIN}.\n\
                 What to do: set {WALLET_PASSWORD_ENV} to a longer passphrase, one you have written \
                 down somewhere physical, and start the pool again.",
                direct.len()
            ));
        }
        return Ok(());
    }
    let Ok(file) = std::env::var(WALLET_PASSWORD_FILE_ENV) else {
        return Ok(());
    };
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => Zeroizing::new(t),
        Err(e) => {
            return Err(format!(
                "{WALLET_PASSWORD_FILE_ENV} names {file}, which cannot be read: {e}.\n\
                 What to do: point it at a readable file holding just the passphrase, or unset it \
                 and use {WALLET_PASSWORD_ENV} instead."
            ));
        }
    };
    let pass = Zeroizing::new(text.trim().to_string());
    if !pass.is_empty() && pass.len() < WALLET_PASSWORD_MIN {
        return Err(format!(
            "the wallet passphrase in {file} is {} character(s); it must be at least \
             {WALLET_PASSWORD_MIN}.\n\
             What to do: put a longer passphrase in that file and start the pool again.",
            pass.len()
        ));
    }
    Ok(())
}

/// Is the key file on disk an encrypted envelope, or a bare private key?
///
/// Read from the FILE, never from the environment. A passphrase that is set but
/// whose migration failed leaves a plaintext key behind, and an operator has to
/// be told what is really on the disk rather than what was intended. This is the
/// same test the wallet loader applies, and it reads only the first few bytes, so
/// no part of a key is copied out of the file.
fn key_file_encrypted(path: &str) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = Zeroizing::new([0u8; 8]);
    let Ok(n) = f.read(&mut head[..]) else {
        return false;
    };
    head[..n].iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{')
}

/// The node's current tip height, or None if it did not answer at all.
///
/// Asked BEFORE the difficulty rule is proved, so "your node is not there" and
/// "your node is on a different chain" are two different messages with two
/// different fixes, instead of one sentence covering both.
fn node_tip(client: &reqwest::blocking::Client, node: &str) -> Option<u64> {
    find_u64(&get_json(client, &format!("{node}/query/latest")), "height")
}

/// Print `text` and stop with the conventional configuration-error status.
fn refuse(text: &str) -> ! {
    eprintln!("{text}");
    std::process::exit(2)
}

/// Refuse an argument the operator typed, with the whole usage text under it.
fn refuse_with_usage(text: &str) -> ! {
    eprintln!("{}\n", usage());
    refuse(text)
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    // Asked for, so it goes to stdout and succeeds: `hbit-pool-server --help >
    // help.txt` has to work like every other program's help.
    if a.iter()
        .skip(1)
        .any(|x| x == "-h" || x == "--help" || x == "/?")
    {
        println!("{}", usage());
        return;
    }
    // Five arguments are required. Nothing is guessed: every one of them decides
    // something an operator cannot see the consequences of afterwards, from which
    // wallet holds other people's money to which chain the work is for.
    if a.len() < 6 {
        refuse_with_usage(&format!(
            "REFUSING to start: {} argument(s) given, 5 are required \
             (<node> <wallet_file> <listen> <share_bits> <chain>).",
            a.len() - 1
        ));
    }
    let node = a[1].trim().trim_end_matches('/').to_string();
    let wallet_file = a[2].trim().to_string();
    let listen = a[3].trim().to_string();
    // How many powers of two easier than a network block a share is. Tune to the
    // miner population and GPU batch size: too small and small miners rarely find
    // a share; too large and a whole GPU batch's best hash always beats it, so
    // credit tracks batch cadence rather than hashrate. ~24 suits GPU batches.
    //
    // A value that does not parse is REFUSED, never quietly replaced by the
    // default: an operator who typed `share_bits` wrong would otherwise be told
    // nothing and would run for weeks on a share size they did not choose.
    let Ok(share_factor) = a[4].trim().parse::<u32>() else {
        refuse_with_usage(&format!(
            "REFUSING to start: <share_bits> must be a whole number between {MIN_SHARE_FACTOR} and \
             {MAX_SHARE_FACTOR} (got `{}`).\n\
             What to do: pass {DEFAULT_SHARE_BITS} unless you have measured otherwise.",
            a[4]
        ));
    };
    if let Err(e) = check_share_factor(share_factor) {
        refuse_with_usage(&format!("REFUSING to start: {e}"));
    }
    // chain is REQUIRED: a mainnet pool run with testnet difficulty (or vice
    // versa) computes the wrong target and every block/share is rejected. Refuse
    // to guess.
    //
    // A testnet node takes its difficulty window and block time from its OWN
    // config file, so accept them spelled out rather than assuming a pair that
    // would make the node reject every block this pool mines.
    let chain = a[5].trim().to_string();
    let Some(params) = ChainParams::parse(&chain) else {
        refuse_with_usage(&format!(
            "REFUSING to start: <chain> must be `mainnet`, `testnet`, or \
             `testnet:<difficulty_adjust_blocks>:<each_block_target_time>` (got `{chain}`).\n\
             What to do: name the chain YOUR node is on. There is no default, because a pool \
             mining on the wrong rule has every block it finds thrown away, forever, with \
             nothing said about it."
        ));
    };
    let settle_secs: u64 = match a.get(6) {
        None => DEFAULT_SETTLE_SECS,
        Some(raw) => {
            let Ok(secs) = raw.trim().parse::<u64>() else {
                refuse_with_usage(&format!(
                    "REFUSING to start: [settle_secs] must be a whole number of seconds between \
                     {MIN_SETTLE_SECS} and {MAX_SETTLE_SECS} (got `{raw}`).\n\
                     What to do: leave it out for the default of {DEFAULT_SETTLE_SECS} ({}).",
                    every(DEFAULT_SETTLE_SECS)
                ));
            };
            secs
        }
    };
    if !(MIN_SETTLE_SECS..=MAX_SETTLE_SECS).contains(&settle_secs) {
        refuse_with_usage(&format!(
            "REFUSING to start: [settle_secs] must be between {MIN_SETTLE_SECS} and \
             {MAX_SETTLE_SECS} (got {settle_secs}).\n\
             Every settlement is a signed transaction that costs a network fee, so paying out \
             faster than that spends the reserve for nothing.\n\
             What to do: leave it out for the default of {DEFAULT_SETTLE_SECS} ({}).",
            every(DEFAULT_SETTLE_SECS)
        ));
    }

    // Name the PRODUCT, not the executable. An operator reading a terminal or
    // pasting a log into a support thread has to be able to say what is running,
    // and a file name does not tell them: this is the HBIT pool.
    println!("== HBIT pool server v{} ==", env!("CARGO_PKG_VERSION"));
    println!("node    = {node}");
    // Settle what protects the key BEFORE a key exists, so a passphrase that is
    // too short or unreadable stops the run instead of panicking halfway through
    // creating the wallet it was supposed to protect.
    if let Err(e) = check_wallet_passphrase() {
        refuse(&format!("REFUSING to start: {e}"));
    }
    // Exactly one process may settle a wallet, enforced by the OS for as long as
    // this one lives. `hbit-pool-payout` takes the SAME lock, so it can never
    // pay out of a wallet this server is already settling: both read the CONFIRMED
    // balance (a payout waiting in the mempool does not reduce it), so each would
    // see the full balance and pay the same PPLNS window a second time.
    let _settle_lock = match acquire_settle_lock(&wallet_file) {
        Ok(l) => l,
        Err(e) => refuse(&format!(
            "REFUSING to start: another hbit-pool-server or hbit-pool-payout already holds \
             {wallet_file} ({e}).\n\
             Two of them would each see the whole wallet balance, each believe it is the only \
             payer, and pay the same shares twice out of your own funds.\n\
             What to do: stop the other one, wait for it to actually exit, then start this again.\n\
             The lock belongs to the running process, not to the file: deleting {} frees nothing \
             and would only let two of them pay at once.",
            settle_lock_path(&wallet_file)
        )),
    };

    let client = http_client();
    // Two different failures with two different fixes, so they get two different
    // messages: a node that is not answering at all, and a node that is answering
    // about a different chain than the one named on the command line.
    let Some(tip) = node_tip(&client, &node) else {
        refuse(&format!(
            "REFUSING to start: no Hacash fullnode answered at {node}.\n\
             Nothing was mined and nothing was paid; the pool cannot serve work without a node.\n\
             What to do: start your fullnode and let it finish syncing, then check that {node} is \
             its API address. The port is the `[server] listen` value in the node's \
             hacash.config.ini, which is 8080 in the config this package ships (so \
             http://127.0.0.1:8080)."
        ));
    };
    // Prove the difficulty rule in force here reproduces the node's OWN tip
    // before serving a single piece of work. Otherwise a chain label that does
    // not match the node's config makes every block the pool finds rejected, and
    // nothing says so: the pool just mines dead work indefinitely.
    if let Err(e) = verify_chain_params(&client, &node, &params) {
        refuse(&format!(
            "REFUSING to start: {e}\n\
             What to do: pass the chain that node is really on as <chain>. If it is a testnet, \
             spell out its own two settings as \
             `testnet:<difficulty_adjust_blocks>:<each_block_target_time>`, copied from that \
             node's hacash.config.ini. Do not work around this: every block the pool found would \
             be thrown away and every miner here would earn nothing."
        ));
    }
    // Bind BEFORE creating a wallet. The commonest first-run mistakes here are a
    // port something else already holds and a listen address with no port in it,
    // and neither of them should leave a real-money key file behind.
    let listener = match TcpListener::bind(&listen) {
        Ok(l) => l,
        Err(e) => refuse(&format!(
            "REFUSING to start: cannot listen on {listen} ({e}).\n\
             What to do: <listen> must be <ip>:<port>, for example 0.0.0.0:9777 to let other \
             machines mine here or 127.0.0.1:9777 to keep it on this one. If the address looks \
             right, something else already holds that port - another hbit-pool-server, or your \
             node - so pick a different port or stop the other program."
        )),
    };

    let wallet_existed = std::path::Path::new(&wallet_file).exists();
    // The ONE read of the key file this process makes. Startup is the only place
    // a wallet may be created or a failure may stop the program, because it is
    // the only place an operator is watching and no money is in flight yet.
    let wallet = Arc::new(load_or_create_wallet(&wallet_file));
    let payout = wallet.readable().to_string();
    // What is REALLY on the disk now, after any migration the loader performed.
    let encrypted = key_file_encrypted(&wallet_file);

    // The header timestamp the previous run was serving. If the chain has not
    // moved since, this run serves byte-identical headers and every worker's
    // in-flight scan pass survives the restart; if it has, the pin does not match
    // and a fresh template is stamped exactly as before.
    let state_file = pool_state_path(&wallet_file);
    let pin = read_stamp_pin(&state_file);
    let Some((tpl, txs_note)) =
        fetch_pool_template(&client, &node, &payout, &params, None, pin.as_ref())
    else {
        refuse(&format!(
            "REFUSING to start: the node at {node} answered, but would not give a block template \
             to mine on.\n\
             What to do: let the node finish syncing (its own log says so) and check it is not \
             shutting down, then start the pool again. Nothing was mined and nothing was paid."
        ));
    };
    let network_target = tpl.target;
    // The factor the operator asked for is not necessarily the factor workers
    // get: the derivation saturates. Refuse to serve, and to distribute real
    // money, on a share nobody had to work for.
    let share_target = pool_core::share_target_hash(tpl.difficulty, share_factor);
    let share_factor_achieved = pool_core::achieved_share_factor(&network_target, &share_target);
    let share_cost = pool_core::share_cost_bits(&share_target);
    if let Err(e) = check_share_target(
        share_factor,
        share_factor_achieved,
        share_cost,
        tpl.difficulty,
    ) {
        refuse(&format!("REFUSING to start: {e}"));
    }

    println!(
        "chain   = {chain} (ASERT at height {})",
        params.asert_height
    );
    println!(
        "height  = {} (template, difficulty {}, {} packed tx(s) from the node)",
        tpl.height,
        tpl.difficulty,
        tpl.txs.bodies.len()
    );
    // Whether workers that were mining before the restart can keep their in-flight
    // work. Worth a line either way: it is the difference between a restart nobody
    // downstream notices and a restart that costs every connected worker the scan
    // pass it was in, and an operator who sees a burst of rejects afterwards should
    // be able to tell from the log which one happened.
    let reused = pin.as_ref().is_some_and(|p| {
        p.height == tpl.height && p.prevhash == tpl.prevhash && p.timestamp == tpl.timestamp
    });
    match reused {
        true => println!(
            "header  = unchanged across the restart (timestamp {} for height {} reused from the \
             state file), so work already in flight at every worker stays valid",
            tpl.timestamp, tpl.height
        ),
        false => println!(
            "header  = freshly stamped (timestamp {} for height {}); any worker still mining the \
             previous header will submit above-target shares until its current scan pass ends",
            tpl.timestamp, tpl.height
        ),
    }
    // Say at startup, not only on the next cycle, whether this pool will mine the
    // node's transactions. Mining empty blocks is the failure that leaves the
    // pool's own payouts stuck in the mempool forever.
    report_packed_txs(txs_note.as_deref());
    println!(
        "{}",
        startup_summary(
            &payout,
            &wallet_file,
            encrypted,
            &node,
            &chain,
            Some(tip),
            &listen,
            PPLNS_WINDOW as u64,
            settle_secs,
            share_factor,
            share_factor_achieved,
        )
    );
    // Last, so it is what an operator comes back to: this run made a wallet that
    // is about to start holding other people's money.
    if !wallet_existed {
        println!("{}", new_wallet_banner(&wallet_file, &payout, encrypted));
    }

    let mut pool = Pool {
        node: node.clone(),
        payout,
        acc: wallet,
        state_file,
        client,
        params,
        share_target,
        tpl,
        share_factor,
        share_factor_achieved,
        share_cost_bits: share_cost,
        // `check_share_target` above already passed on exactly these figures, and
        // `refuse` exits, so reaching here means healthy. Every later template
        // change re-derives this from the same function.
        share_halt: None,
        network_target,
        pending_cache: String::new(),
        workers: HashMap::new(),
        next_en: 0,
        pplns: Pplns::new(PPLNS_WINDOW, pplns_horizon_ms(settle_secs)),
        accepted: 0,
        blocks: 0,
        orphaned: 0,
        seen: HashSet::new(),
        submitted: Vec::new(),
        immature: Vec::new(),
        unsaved: 0,
        state_seq: 0,
        settle_pending_txs: Vec::new(),
        payout_records: Vec::new(),
        inflight_units: 0,
        owed: Vec::new(),
        // A ledger with no start time would report "paid since the beginning of
        // time". `load_state` replaces this with the stored one if there is one.
        paid: PaidLedger::started(curtimes()),
        matured: None,
        matured_current: false,
        settle_secs,
        stall_since: None,
        bad_streak: HashMap::new(),
        // Startup counts as a header change unless the pin above reproduced the
        // previous one. Workers that were mining a moment ago are hashing whatever
        // this process decided to serve, and until that settles the pool has no
        // grounds to say anything about them.
        tpl_changed_at_ms: pool_core::now_ms(),
        rates: HashMap::new(),
        share_log: ShareLog::default(),
    };
    pool.load_state();
    if pool.paid.since == 0 {
        // A state file written before the ledger existed: start counting now
        // rather than claim a total that reaches back further than it does.
        pool.paid.since = curtimes();
    }
    pool.rebuild_pending_cache();
    let pool = Arc::new(Mutex::new(pool));

    // Background: keep the template current with the chain tip and confirm our
    // submitted blocks. All node HTTP happens OFF the pool lock, so miners are
    // never stalled by it. This also advances work when the NETWORK finds a
    // block, not only when we do. The whole loop body is panic-isolated so a
    // single transient fault can never kill this long-lived thread.
    {
        let pool = pool.clone();
        let (client, node, payout, params) = {
            let p = plock(&pool);
            (
                p.client.clone(),
                p.node.clone(),
                p.payout.clone(),
                p.params.clone(),
            )
        };
        std::thread::spawn(move || {
            let mut tick: u64 = 0;
            loop {
                // The wallet valuation behind every miner's PENDING figure is
                // refreshed here, on a slow multiple of the template cycle: it is
                // one extra node call, and it must never happen on a request.
                let money = tick % MONEY_REFRESH_CYCLES == 0;
                tick = tick.wrapping_add(1);
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    template_cycle(&pool, &client, &node, &payout, &params, money);
                }));
                if let Err(e) = r {
                    eprintln!("[template] cycle panicked, continuing: {e:?}");
                }
                // The accepted-share summary is due on a CLOCK, not on the next
                // share. Without this the shares a fleet submitted just before
                // it stopped are never counted out loud - the log simply ends
                // mid-count - and an operator reading it cannot tell a fleet
                // that went quiet from a pool that stopped accepting work. The
                // line is composed under the lock and printed off it: stdout can
                // block, and every miner request is serialized behind this
                // mutex.
                let due = plock(&pool).share_log.due(pool_core::now_ms());
                if let Some(line) = due {
                    println!("{line}");
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
    }

    // Automatic settlement on a timer.
    {
        let p = pool.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(settle_secs));
                // One bad settle cycle (poisoned lock, wallet issue, future
                // refactor) must never permanently kill payouts. This only holds
                // while everything the cycle does is unwindable: nothing under
                // here may `process::exit`, or the catch is decoration and the
                // pool dies with payouts half-issued. That is why the signing key
                // is read at startup and held on `Pool`, never re-read here.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| settle_once(&p)));
                if let Err(e) = r {
                    eprintln!("[settle] cycle panicked, continuing: {e:?}");
                }
            }
        });
    }

    println!("listening on {listen}\n");
    for stream in listener.incoming() {
        let s = match stream {
            Ok(s) => s,
            // A single accept() error (e.g. EMFILE under load) must not tear down
            // the whole listener - log and keep serving.
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };
        if CONNS.load(Relaxed) >= MAX_CONNS {
            continue; // drop: s closes as it goes out of scope
        }
        let ip = s.peer_addr().ok().map(|a| a.ip());
        // Per-IP admission: one source cannot hold more than MAX_PER_IP slots,
        // so a single host cannot pin every connection with long-polls.
        if let Some(ip) = ip {
            let mut m = per_ip_lock();
            let c = m.entry(ip).or_insert(0);
            if *c >= MAX_PER_IP {
                continue; // drop this connection from a noisy IP
            }
            *c += 1;
        }
        CONNS.fetch_add(1, Relaxed);
        let guard = ConnGuard { ip };
        let p = pool.clone();
        // NEVER `thread::spawn` here: it is `Builder::spawn(..).expect(..)`, so it
        // PANICS when the OS refuses a new thread (RLIMIT_NPROC, exhausted address
        // space), and that panic unwinds straight out of main and kills the whole
        // pool. Treat a spawn failure as backpressure instead: dropping the
        // closure releases the ConnGuard (global + per-IP slot) and closes `s`.
        if let Err(e) = std::thread::Builder::new()
            .stack_size(HANDLER_STACK_BYTES)
            .spawn(move || {
                let _g = guard; // releases the slot on return AND on unwind
                handle(s, p);
            })
        {
            eprintln!("[accept] thread spawn failed, dropping connection: {e}");
        }
    }
}

/// What a money refresh may publish, given what the node said about the wallet.
///
/// `None` means "publish nothing this cycle": the caller keeps the last figure
/// it could stand behind and flags it STALE. It is the only safe reading of an
/// answer the pool cannot value, because the alternative reads to a miner as
/// "your pool owes you nothing" while its shares age out of the payout window.
///
/// `Some(0)` is a different statement, and a real one: the wallet is at or below
/// the fee reserve and the pool is standing behind that figure.
fn refreshed_money(bal: &BalanceAnswer, immature_units: u64) -> Option<u64> {
    let units = bal.units()?;
    // `distributable_units` returning None here is a KNOWN nothing (the balance
    // is at or below the reserve), not a balance that could not be valued.
    Some(distributable_units(units, immature_units, SETTLE_RESERVE_UNITS).unwrap_or(0))
}

/// One iteration of the background template/confirmation loop (panic-isolated by
/// the caller). Refreshes the template on a height change and confirms/orphans
/// our submitted blocks. All node HTTP is done before taking the pool lock.
fn template_cycle(
    pool: &Arc<Mutex<Pool>>,
    client: &reqwest::blocking::Client,
    node: &str,
    payout: &str,
    params: &ChainParams,
    refresh_money: bool,
) {
    // Cloning the live template is cheap (its transaction set is behind an Arc)
    // and lets an unchanged tip skip re-downloading a whole block of bodies.
    let (current, pending, immature) = {
        let p = plock(pool);
        (p.tpl.clone(), p.submitted.clone(), p.immature.clone())
    };
    // No disk pin here: the live template IS the pin while the pool is running,
    // and `fetch_pool_template` derives it from `current`.
    let fresh = fetch_pool_template(client, node, payout, params, Some(&current), None);
    if let Some((_, why)) = fresh.as_ref() {
        report_packed_txs(why.as_deref());
    }
    let fresh = fresh.map(|(t, _)| t);
    let tip = fresh.as_ref().map(|t| t.height.saturating_sub(1));
    // A block the node refused leaves no other trace than the tip never reaching
    // its height, so watch for exactly that and say it out loud.
    let stalled = {
        let mut st = BLOCK_STALL.lock().unwrap_or_else(|e| e.into_inner());
        note_block_stalls(&mut st, &pending, tip)
    };
    for (h, hx) in stalled {
        eprintln!(
            "[block] the chain has STILL not reached height {h}, long after we submitted our \
             block {} there. /submit/block validates asynchronously and answers before the \
             verdict, so a refusal is silent: this block was almost certainly rejected and its \
             whole reward is lost. If the pool is packing the node's transactions, the likeliest \
             cause is that one of them was no longer valid by the time the block was submitted.",
            hex::encode(hx)
        );
    }
    // One node query per height, shared by the confirm/orphan tally and by the
    // coinbase-maturity gate below.
    let mut heights: Vec<u64> = pending
        .iter()
        .map(|(h, _)| *h)
        .chain(immature.iter().map(|e| e.height))
        .collect();
    heights.sort_unstable();
    heights.dedup();
    let mut chain_hash: HashMap<u64, String> = HashMap::new();
    for h in heights {
        if tip.map(|t| h > t).unwrap_or(true) {
            continue; // not buried yet, or no tip this cycle
        }
        let j = get_json(client, &format!("{node}/query/block/intro?height={h}"));
        if let Some(hx) = find_str(&j, "hash") {
            chain_hash.insert(h, hx);
        }
    }
    // A block counts as confirmed only once the chain has BURIED it while still
    // holding our hash. Finalizing it the moment it merely occupies the tip (0
    // blocks stacked on top) also stops us watching it - exactly when a reorg is
    // most likely - so an orphan after that point could never be detected and
    // blocks_confirmed would over-count against the chain for good.
    let mut confirmed = Vec::new();
    let mut orphaned = Vec::new();
    for (h, ours) in &pending {
        match chain_hash.get(h) {
            Some(cur) if *cur == hex::encode(ours) => {
                if buried_deep(tip, *h) {
                    confirmed.push((*h, *ours));
                }
                // Not buried yet: keep watching it, a reorg can still flip it.
            }
            Some(cur) => {
                orphaned.push((*h, *ours));
                println!("[reorg] our block {h} orphaned (chain holds {cur})");
            }
            None => {} // node has not stored it yet; keep waiting
        }
    }
    // Coinbase maturity: a found block's reward stays held back until the chain
    // still holds OUR hash COINBASE_MATURITY_DEPTH blocks later. Releasing it any
    // earlier means the pool can pay out income that a reorg then takes back,
    // while the payout transaction that spent it stays valid on the new chain.
    let mut released: Vec<(u64, [u8; 32])> = Vec::new();
    for e in &immature {
        match chain_hash.get(&e.height) {
            Some(cur) if *cur == hex::encode(e.hash) => {
                if buried_deep(tip, e.height) {
                    released.push((e.height, e.hash));
                }
            }
            // Orphaned: that income never lands in the balance, so there is
            // nothing left to hold back.
            Some(_) => released.push((e.height, e.hash)),
            None => {}
        }
    }
    // Value the pool wallet OFF the lock, so `/earnings` can answer a PENDING
    // question without any node call at all. Balance FIRST and the hold-back
    // after, exactly as settlement does: a block found in between then shows up
    // in the hold-back but not yet in the balance, which errs towards reporting
    // LESS as owed.
    let money = if refresh_money {
        // The node has to prove it is answering BEFORE its balance is read, the
        // same proof-of-life the manual payout tool takes. The figure published
        // here is what every miner polling /earnings is shown, so the difference
        // between "your pool owes you nothing" and "your pool cannot see its
        // wallet" has to be said in those words, once, and not left as a zero
        // that is re-published every 30 seconds for the length of the outage.
        if node_tip(client, node).is_none() {
            eprintln!(
                "[money] no Hacash fullnode answered at {node}, so the pool cannot value its own \
                 wallet. The last figure it could stand behind is kept and reported to miners as \
                 STALE. This is not a zero: nothing has been paid or unpaid by it."
            );
            None
        } else {
            let bal = balance(client, node, payout);
            // The hold-back as it stands. A block found in the last few minutes
            // may not have had its transaction fees counted yet (settlement does
            // that, against the node), so this figure can run a few units ahead
            // of what will actually be paid. It is deliberately not corrected
            // here: nothing is spent off it, and the alternative is one node
            // call per packed transaction every thirty seconds.
            let immature_units: u64 = plock(pool).immature.iter().map(|e| e.units).sum();
            let money = refreshed_money(&bal, immature_units);
            if money.is_none() {
                eprintln!(
                    "[money] the pool cannot value its own wallet ({bal}). The last good figure is \
                     kept and reported to miners as STALE rather than replaced with a zero."
                );
            }
            money
        }
    } else {
        None
    };

    let mut shot = None;
    let mut degraded: Option<String> = None;
    let mut resumed = false;
    let mut tpl_changed = false;
    {
        let mut p = plock(pool);
        if refresh_money {
            p.matured_current = money.is_some();
            if let Some(units) = money {
                p.matured = Some(Matured {
                    units,
                    at: curtimes(),
                });
            }
        }
        if let Some(t) = fresh {
            // Replace the template when the tip changes: either a new height, or
            // a same-height reorg (different prev-hash). At the same height and
            // same prev-hash the timestamp/difficulty are fixed, so keeping the
            // template valid keeps every worker's in-flight share valid.
            //
            // The packed transaction set is pinned to the template for exactly
            // the same reason, and that is now load-bearing: the set determines
            // the merkle root, so swapping it mid-height would make every share
            // already found for this height - and every batch still running -
            // hash a header the pool no longer reconstructs. Every one of them
            // would be rejected. The node behaves the same way (it repacks only
            // when its own tip moves), so nothing is lost by waiting: a
            // transaction that arrives mid-height is packed into the next block.
            let changed = t.height != p.tpl.height || t.prevhash != p.tpl.prevhash;
            if changed {
                let was_halted = p.share_halt.is_some();
                p.tpl = t;
                p.network_target = p.tpl.target;
                // Re-derive the share target from the new difficulty so shares stay
                // a fixed fraction of a block as difficulty moves - and with it the
                // verdict on whether that target can be accounted on at all. A
                // difficulty FALL can push the derived share target into saturation
                // while the pool is already serving miners, and from that moment
                // credit tracks submission rate, not hashrate. Startup refuses to
                // start on it; here the pool holds other people's money already, so
                // `share_halt` binds the credit path and the settlement instead.
                p.recompute_share_target();
                // Log only on a transition. The template loop runs every couple of
                // seconds, and a warning printed every cycle is a warning nobody
                // reads.
                match (&p.share_halt, was_halted) {
                    (Some(why), false) => degraded = Some(why.clone()),
                    (None, true) => resumed = true,
                    _ => {}
                }
                let height = p.tpl.height;
                prune_seen(&mut p.seen, height);
                p.rebuild_pending_cache();
                // Every connected worker is now hashing a header the pool has
                // replaced, for as long as its current scan pass runs. Record when,
                // so the above-target streak line reports that fact instead of
                // asserting a fault in the worker.
                p.tpl_changed_at_ms = pool_core::now_ms();
                tpl_changed = true;
            }
        }
        p.blocks += confirmed.len() as u64;
        p.orphaned += orphaned.len() as u64;
        p.submitted
            .retain(|e| !confirmed.contains(e) && !orphaned.contains(e));
        if !released.is_empty() {
            // Matched on (height, hash) alone, NEVER on the whole entry: the
            // settlement thread can fold a block's transaction fees into `units`
            // between the snapshot above and this lock, and a whole-entry match
            // would then fail to release a block the chain has already buried.
            // Its income would be held back for the life of the process.
            p.immature.retain(|e| {
                !released
                    .iter()
                    .any(|(h, hx)| *h == e.height && *hx == e.hash)
            });
        }
        // Block bookkeeping changed: snapshot it here, write it below with the
        // lock released.
        let books = !confirmed.is_empty() || !orphaned.is_empty() || !released.is_empty();
        // A new template is also written out, so a restart before the next share
        // reproduces THIS header rather than the previous height's. Not fsynced on
        // its own account: losing it costs one height's in-flight worker work,
        // which is what happened on every restart before the stamp was persisted
        // at all, whereas an fsync per block on the template thread is a real cost
        // paid every time.
        if books || tpl_changed {
            shot = p.state_shot(books);
        }
    }
    if let Some(why) = degraded {
        eprintln!(
            "[share] STOPPED crediting shares and STOPPED settling: {why}\n\
             Nothing is being paid and no new share is being credited. Shares already in the \
             window keep what they earned; crediting and settlement resume by themselves if the \
             difficulty recovers. Miners are told this on /terms and on every submission."
        );
    }
    if resumed {
        println!(
            "[share] the difficulty recovered: shares are being credited again and settlement \
             has resumed"
        );
    }
    flush_state(shot);
}

/// Report whether this pool is mining the node's transactions or a lone
/// coinbase, without turning the log into one line every two seconds.
///
/// A coinbase-only block is the defect this reporting exists for: it earns no
/// transaction fees, and on a chain where this pool is the only miner it means
/// the pool's OWN payout transactions can never confirm. They sit in the node's
/// mempool while block after block is mined carrying nothing.
fn report_packed_txs(why: Option<&str>) {
    let mut st = TX_WARN.lock().unwrap_or_else(|e| e.into_inner());
    match why {
        Some(why) => {
            if should_warn_now(&mut st, why, Instant::now()) {
                eprintln!(
                    "[template] mining WITHOUT the node's transactions: {why}.\n\
                     Every block this pool finds will carry nothing but its coinbase, so it \
                     collects no transaction fees, and on a chain where this pool is the only \
                     miner its OWN payouts can never confirm - they wait in the mempool while \
                     every block is mined with 0 transactions.\n\
                     Fix: the node must serve /query/miner/pending, which needs `enable = true` \
                     and a reward address under `[miner]` in the node's config."
                );
            }
        }
        None => {
            if st.take().is_some() {
                println!("[template] the node's packed transactions are available again");
            }
        }
    }
}

/// Print the packed-transaction warning now? The first occurrence and every
/// change of reason go out immediately; an unchanged reason repeats at most once
/// every `TX_WARN_REPEAT` so the operator is reminded without being buried.
fn should_warn_now(state: &mut Option<(String, Instant)>, why: &str, now: Instant) -> bool {
    match state {
        Some((prev, at)) if prev == why && now.duration_since(*at) < TX_WARN_REPEAT => false,
        _ => {
            *state = Some((why.to_string(), now));
            true
        }
    }
}

/// Count how long each submitted block has gone without the chain reaching its
/// height, and return the ones that just crossed `BLOCK_STALL_CYCLES`.
///
/// Counted ONLY while the tip is actually known, so a node that is merely
/// unreachable never trips it. An entry clears the moment the chain reaches that
/// height (accepted, or orphaned by someone else's block, which the confirm
/// tally reports separately) or the moment the pool stops tracking the block.
fn note_block_stalls(
    state: &mut HashMap<(u64, [u8; 32]), u32>,
    pending: &[(u64, [u8; 32])],
    tip: Option<u64>,
) -> Vec<(u64, [u8; 32])> {
    let Some(tip) = tip else {
        return Vec::new();
    };
    state.retain(|key, _| pending.contains(key));
    let mut shout = Vec::new();
    for key in pending {
        if key.0 <= tip {
            state.remove(key);
            continue;
        }
        let n = state.entry(*key).or_insert(0);
        *n += 1;
        // Exactly at the threshold: one loud line per lost block, not a stream.
        if *n == BLOCK_STALL_CYCLES {
            shout.push(*key);
        }
    }
    shout
}

/// Has the chain stacked COINBASE_MATURITY_DEPTH blocks on top of height `h`?
/// A `None` tip means we could not read the chain this cycle, so nothing counts
/// as buried: both callers must err towards keeping a block under observation.
fn buried_deep(tip: Option<u64>, h: u64) -> bool {
    tip.map(|t| t.saturating_sub(h) >= COINBASE_MATURITY_DEPTH)
        .unwrap_or(false)
}

/// Drop replay-protection entries that can no longer be resubmitted (strictly
/// lower heights) and KEEP every entry at the current height.
///
/// Clearing the whole set on a template swap looks harmless because a swap
/// usually means a new height, but it also fires on a SAME-height reorg. If the
/// chain then flaps back to the original prev-hash the template is byte-identical
/// again, and an emptied set re-admits solutions that were already credited -
/// letting a miner double-count their own shares in the PPLNS window and take
/// payout funds from everyone else.
fn prune_seen(seen: &mut HashSet<(u64, [u8; 32], u32)>, height: u64) {
    seen.retain(|(h, _, _)| *h >= height);
}

/// What became of one settlement chunk. Only `Delivered` is money in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkOutcome {
    /// The node accepted it AND confirms it holds the transaction.
    Delivered,
    /// The node refused it, or accepted the bytes and then never took the
    /// transaction. Nothing was paid and nothing was relayed.
    Failed,
    /// Submitted, but we could not get the node's verdict. It may be in flight,
    /// so it stays in the pending ledger and must NOT be counted as paid.
    Unresolved,
}

/// Running tally of one settlement pass.
///
/// Only chunks the node actually holds may be reported as paid. Counting an
/// attempted chunk tells the operator (and anything built on these logs) that
/// money moved when it did not, which is exactly how a stalled payout goes
/// unnoticed while miners keep mining for nothing.
#[derive(Debug, Default, PartialEq, Eq)]
struct SettleTally {
    recipients: usize,
    units: u64,
    txs: usize,
    failed_txs: usize,
    failed_units: u64,
    unresolved_txs: usize,
    unresolved_units: u64,
}

impl SettleTally {
    fn record(&mut self, outcome: ChunkOutcome, pushed: usize, units: u64) {
        match outcome {
            ChunkOutcome::Delivered => {
                self.recipients += pushed;
                self.units += units;
                self.txs += 1;
            }
            ChunkOutcome::Failed => {
                self.failed_txs += 1;
                self.failed_units += units;
            }
            ChunkOutcome::Unresolved => {
                self.unresolved_txs += 1;
                self.unresolved_units += units;
            }
        }
    }

    /// Did every chunk reach the node? Anything else means money is still owed.
    fn all_delivered(&self) -> bool {
        self.failed_txs == 0 && self.unresolved_txs == 0
    }

    fn attempted(&self) -> bool {
        self.txs > 0 || self.failed_txs > 0 || self.unresolved_txs > 0
    }

    /// The closing line. Never claims a payout happened: a transaction the node
    /// holds is submitted, not paid, until a block includes it.
    fn summary(&self) -> String {
        let mut s = format!(
            "[settle] settlement submitted: {} recipient(s), {} unit(s) across {} tx(s) the node \
             holds (NOT paid until they confirm on-chain)",
            self.recipients, self.units, self.txs
        );
        if self.failed_txs > 0 {
            s.push_str(&format!(
                "; FAILED: {} tx(s) covering {} unit(s) never reached the node and are still owed. \
                 Those exact rows are on the owed ledger and the next cycle pays THEM first, \
                 before it splits any fresh income",
                self.failed_txs, self.failed_units
            ));
        }
        if self.unresolved_txs > 0 {
            s.push_str(&format!(
                "; UNVERIFIED: {} tx(s) covering {} unit(s) were submitted but the node's verdict \
                 could not be read, so they are NOT counted as paid",
                self.unresolved_txs, self.unresolved_units
            ));
        }
        s
    }
}

/// Drop a payout hash the node definitively does not hold from the shared
/// pending ledger, and put what it was going to pay onto the owed ledger.
///
/// Safe ONLY for a definitive "the node never took it" - `Admission::Missing`, or
/// a non-zero `ret` from the node's own validator. The node inserts into its
/// mempool before it relays, so a transaction it never inserted was never
/// broadcast either and cannot come back from a peer to be paid twice. A TIMEOUT
/// is not that verdict and must never come here.
///
/// The rows do not simply return to the pot. They name the miners this chunk was
/// for, and the next cycle would otherwise re-split that money over the whole
/// live window: the miners whose chunks went through would take a second cut of
/// it, and the miners in this chunk would get a fraction of what they are owed.
fn owe_back_failed_payout(pool: &Arc<Mutex<Pool>>, txhash: &str) {
    let shot = {
        let mut p = plock(pool);
        p.settle_pending_txs.retain(|h| h != txhash);
        // Nothing was paid, so nothing is credited. Reborrow so both ledgers move
        // in ONE locked step and one snapshot: a unit is never in flight and owed
        // at the same time, and never neither.
        let pp = &mut *p;
        if let Some(rec) = drop_payout(&mut pp.payout_records, txhash) {
            owe_rows(&mut pp.owed, &rec.rows);
        }
        p.rebuild_inflight();
        p.state_shot(true)
    };
    flush_state(shot);
}

/// Fold one block's transaction fees into the hold-back for that block, once.
/// Answers whether it changed anything.
///
/// Matched on (height, hash) and guarded by `fees_counted`, because the node is
/// re-read on every settlement cycle until it answers: folding the same fees in
/// twice would hold back money that does not exist and quietly stop paying the
/// miners it belongs to, for as long as the block stays immature.
fn fold_block_fees(
    immature: &mut [Immature],
    height: u64,
    hash: &[u8; 32],
    fee_units: u64,
) -> bool {
    for e in immature.iter_mut() {
        if e.height == height && e.hash == *hash && !e.fees_counted {
            e.units = e.units.saturating_add(fee_units);
            e.fees_counted = true;
            return true;
        }
    }
    false
}

/// Total income the pool is holding back, in units of 0.1 HAC, having first
/// asked the node what each block's transactions paid in fees.
///
/// `None` means one of those blocks could not be valued, and NOTHING may be
/// settled this cycle. The chain credits a block's whole fee income to the
/// coinbase address, which is this same wallet, so a block whose fees are still
/// unknown is money already sitting in the balance with nothing holding it back.
/// Paying against it distributes that income at zero confirmations, and if the
/// block is then orphaned the money is gone from the chain while the payout that
/// spent it is still valid: the operator funds the difference themselves.
///
/// A block the node says is NOT on the chain is not a refusal. It credited
/// nothing at all, so it has no fees to hold back, and the confirmation loop
/// releases its entry once another block takes that height.
fn count_immature_fees(
    pool: &Arc<Mutex<Pool>>,
    client: &reqwest::blocking::Client,
    node: &str,
) -> Option<u64> {
    // Off the lock: this is one node call per block, plus one per transaction in
    // it, and every miner request is serialized behind this mutex.
    let uncounted: Vec<(u64, [u8; 32])> = {
        let p = plock(pool);
        p.immature
            .iter()
            .filter(|e| !e.fees_counted)
            .map(|e| (e.height, e.hash))
            .collect()
    };
    let mut counted: Vec<(u64, [u8; 32], u64)> = Vec::new();
    for (height, hash) in uncounted {
        match block_fees(client, node, height, &hex::encode(hash)) {
            BlockFees::Counted(fee) => counted.push((height, hash, fee)),
            BlockFees::NotOnChain => {}
            BlockFees::Unknown(why) => {
                eprintln!(
                    "[settle] the node could not say what our block at height {height} paid this \
                     pool in transaction fees ({why}). That fee income is already in the pool \
                     wallet, and paying against a balance that still contains it would hand it to \
                     miners at zero confirmations - money the operator would have to fund out of \
                     their own pocket if the block is orphaned. Nothing is settled this cycle."
                );
                return None;
            }
        }
    }
    let (shot, total) = {
        let mut p = plock(pool);
        let changed = !counted.is_empty();
        for (height, hash, fee) in counted {
            fold_block_fees(&mut p.immature, height, &hash, fee);
        }
        let total = p
            .immature
            .iter()
            .map(|e| e.units)
            .fold(0u64, |a, b| a.saturating_add(b));
        // Persist it: a crash between counting a block's fees and paying must
        // not restart from the subsidy alone and distribute them.
        (changed.then(|| p.state_shot(true)).flatten(), total)
    };
    flush_state(shot);
    Some(total)
}

/// Has an outstanding payout waited long enough to be a FAULT rather than normal
/// progress?
///
/// Two arms, because a payout waits on blocks but a stalled chain produces none.
/// The block arm catches "the chain is moving and nothing includes it"; the
/// wall-clock arm catches "the tip has not moved at all", which the block arm
/// would sit on in silence forever while every payout stays frozen and nobody is
/// paid. Neither arm looks at the settlement interval: that is an operator
/// setting, and measuring a chain event with it is what made the old check fire
/// on every healthy payout.
fn payout_stalled(blocks_waited: u64, secs_waited: u64, target_block_secs: u64) -> bool {
    if blocks_waited >= STALLED_PAYOUT_BLOCKS {
        return true;
    }
    // `max(1)` only guards a nonsense chain parameter; every real one is seconds
    // per block, and a zero would make the time arm fire instantly on a pool
    // that is perfectly healthy.
    let budget = STALLED_PAYOUT_BLOCKS
        .saturating_mul(target_block_secs.max(1))
        .saturating_mul(STALLED_PAYOUT_TIME_SLACK);
    secs_waited >= budget
}

/// What the node last said about the payouts a settlement cycle could not
/// resolve: counted, never inferred.
///
/// The warning built from this is the only thing that sends an operator to look
/// at their pool, so it may state nothing this pool has not measured. The text
/// it replaced asserted a cause it never tested - that the pool's blocks carry
/// only their coinbase, so only another miner could ever confirm a payout - and
/// that stopped being true the day the pool started packing the node's
/// transactions. A wrong cause costs more than silence: the operator goes off to
/// fix peering on a node that is peered fine, while the real reason nobody is
/// being paid goes unexamined.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct StallWait {
    /// Payouts the node is holding in its mempool: waiting for ANY block to
    /// include them.
    in_mempool: usize,
    /// Shallowest burial depth among payouts already mined, if any: waiting only
    /// for the chain to grow on top of them.
    confirming: Option<u64>,
    /// Payouts the node no longer holds. Re-broadcast or frozen, and the lines
    /// printed per transaction above say which.
    off_mempool: usize,
    /// Payouts whose state the node would not report. NOTHING is claimed about
    /// these; unresolved is not a verdict.
    unreadable: usize,
}

impl StallWait {
    fn note(&mut self, st: PayoutTxState) {
        match st {
            PayoutTxState::Pending => self.in_mempool += 1,
            PayoutTxState::Confirming(d) => {
                self.confirming = Some(self.confirming.map_or(d, |had| had.min(d)));
            }
            PayoutTxState::Gone => self.off_mempool += 1,
            PayoutTxState::Unknown => self.unreadable += 1,
            // Burial is the resolution, not a wait: a buried payout is credited
            // and dropped from tracking, so it never reaches this summary.
            PayoutTxState::Buried(_) => {}
        }
    }

    /// One clause per thing actually being waited on.
    fn waiting_on(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.in_mempool > 0 {
            parts.push(format!(
                "{} in the node's mempool, waiting for a block to include it",
                self.in_mempool
            ));
        }
        if let Some(d) = self.confirming {
            parts.push(format!(
                "mined but not yet buried: the shallowest is {d} block(s) deep and needs {} more \
                 to reach the {PAYOUT_MATURITY_DEPTH}-block burial depth",
                PAYOUT_MATURITY_DEPTH.saturating_sub(d)
            ));
        }
        if self.off_mempool > 0 {
            parts.push(format!(
                "{} the node no longer holds (see the per-transaction lines above)",
                self.off_mempool
            ));
        }
        if self.unreadable > 0 {
            parts.push(format!(
                "{} whose state the node would not report, so nothing is claimed about them",
                self.unreadable
            ));
        }
        if parts.is_empty() {
            // Every arm that keeps a payout also records it, so this is
            // unreachable; say so rather than print an empty clause.
            return "nothing this pool was able to classify".to_string();
        }
        parts.join("; ")
    }
}

/// The operator-facing stall line, built only from measured facts: how long the
/// payout has been waiting, what it is waiting on, and how many transactions
/// this pool's own next block would carry.
///
/// `packed_txs` is what makes the last part honest. Whether the pool's own
/// blocks can confirm its own payouts is CHECKED here instead of asserted, so
/// the "only another miner can include it" reading is printed only on a pool
/// whose template really is coinbase-only.
fn stall_warning(
    blocks_waited: u64,
    secs_waited: u64,
    wait: &StallWait,
    packed_txs: usize,
) -> String {
    let waiting = wait.waiting_on();
    let own_blocks = if packed_txs == 0 {
        "This pool's own next block would carry nothing but its coinbase (0 transactions packed \
         from the node), so a block THIS pool mines cannot include the payout: it confirms only \
         when another miner packs it."
            .to_string()
    } else {
        format!(
            "This pool's own next block carries {packed_txs} transaction(s) packed from the node, \
             so a block THIS pool mines can include the payout itself; being the only miner on the \
             chain does not explain the wait."
        )
    };
    format!(
        "[settle] WARNING: a payout this pool submitted has not resolved in {blocks_waited} \
         block(s) / {secs_waited}s, and every later payout is frozen behind it, so nobody is being \
         paid. Waiting on: {waiting}. {own_blocks} What to check: that the node's tip is advancing \
         (it moved {blocks_waited} block(s) over that wait) and that the node has peers."
    )
}

/// Pay every miner their PPLNS share of the pool's spendable balance. Splits the
/// distributable balance over PAYABLE workers only, then submits one or more
/// transactions (chunked to <=PAYOUT_CHUNK actions each) so a large payout is
/// never rejected by the node's 200-action limit. Idempotent across restarts via
/// the persisted `settle_pending_txs`.
fn settle_once(pool: &Arc<Mutex<Pool>>) {
    let (node, acc, counts, pending_txs, tracked) = {
        let p = plock(pool);
        (
            p.node.clone(),
            // The wallet startup loaded, NOT a fresh read of the key file. This
            // cycle values a balance and signs transfers out of it, and both have
            // to be the address the pool mines to; see `Pool::acc`.
            p.acc.clone(),
            settlement_credit(&p, pool_core::now_ms()),
            p.settle_pending_txs.clone(),
            // The rows and signed bytes behind those hashes. Needed OFF the lock:
            // deciding what a "the node does not know this hash" answer means
            // depends on whether the node ever held that transaction, and if it
            // did, the only safe move is to re-broadcast the identical bytes.
            p.payout_records.clone(),
        )
    };
    let client = http_client();

    // Nothing in this cycle is decidable without the node, so ask it for its tip
    // FIRST - the same proof-of-life the manual payout tool takes before it
    // values anything. Without it, every query below fails in its own way: the
    // payout poll reads "unresolved" and burns a stall warning, and the balance
    // read used to come back as an empty string that valued as a confident zero.
    // The cost of getting that wrong is that miners are told they are owed
    // nothing while their shares age out of the payout window unpaid.
    // The height is kept, not just tested: a payout waits on BLOCKS, so this tip
    // is the only measure of how long an unresolved payout has really been
    // waiting. Measured in settlement cycles instead, the answer says more about
    // the operator's `settle_secs` than about the chain.
    let Some(tip) = node_tip(&client, &node) else {
        eprintln!(
            "[settle] no Hacash fullnode answered at {node}; skipping this settlement cycle. \
             Nothing was paid, nothing was resolved, and no accounting changed. Miners are told \
             their pending figure is STALE rather than zero."
        );
        // The pool cannot see its own wallet, so `/earnings` must stop claiming
        // its last figure is current.
        plock(pool).matured_current = false;
        return;
    };

    // Resolve any outstanding payouts FIRST, using the node's own view of each tx
    // rather than the wallet balance. Correct even if a submit ACK was lost and
    // even though the same wallet keeps earning coinbase income.
    //
    // This runs BEFORE the "nothing to settle" exit on purpose. A pool with an
    // empty share window still has to finish resolving the payouts it already
    // made: they are what turns a miner's IN FLIGHT into PAID, and skipping the
    // poll left a miner's last payout showing as in flight forever.
    //
    // This guard must fail SAFE. Only a definitive node verdict resolves a hash:
    // an unreachable node, a timeout or an answer we cannot parse keeps the hash
    // and skips the cycle, and a payout confirmed shallower than
    // PAYOUT_MATURITY_DEPTH stays tracked because a reorg could still return it
    // to the mempool. Anything else re-opens the double-payout window.
    if !pending_txs.is_empty() {
        let mut still = Vec::new();
        let mut buried: Vec<String> = Vec::new();
        let mut gone: Vec<String> = Vec::new();
        // What every payout that stays tracked is waiting on, as the node
        // reported it this cycle. The stall warning is built from this and from
        // nothing else, so it can never name a cause that was not observed.
        let mut wait = StallWait::default();
        // Payouts the node itself has just told us it is holding. Recorded,
        // because `node_holds` is the pool's only memory that a transaction
        // reached the network, and until now it was written in ONE place: right
        // after a submit whose verification came back `Held`. A payout submitted
        // through a timeout and only seen in the mempool on a later cycle stayed
        // marked "never held" for ever, so the day the node restarted and
        // answered `Gone` the pool read it as never relayed. It is also the
        // `node_holds` every miner is shown on `/earnings`.
        let mut seen_held: Vec<String> = Vec::new();
        for hx in &pending_txs {
            let j = get_json(&client, &format!("{node}/query/transaction?hash={hx}"));
            // Never slice mid-character: these come off disk, and a corrupt
            // ledger entry must not panic the settlement thread.
            let short = hx.get(..16).unwrap_or(hx);
            match classify_payout_tx(&j) {
                // The node has buried it: this, and ONLY this, is what turns
                // money that was in flight into money that was paid.
                PayoutTxState::Buried(_) => buried.push(hx.clone()),
                // "I do not know that hash" is NOT "nothing was paid". The
                // mempool lives in memory, so a node restart empties it and a
                // transaction the node validated, accepted AND RELAYED reads
                // exactly like one it never took. What separates them is whether
                // this pool ever saw the node hold it.
                PayoutTxState::Gone => {
                    let rec = tracked.iter().find(|r| r.hash == *hx);
                    match gone_action(rec) {
                        GoneAction::Forget => {
                            eprintln!(
                                "[settle] payout tx {short} is unknown to the node, and this pool \
                                 has neither its signed bytes nor any sighting of the node holding \
                                 it: nothing can have been relayed and nobody was paid. Its rows \
                                 go back on the owed ledger and are paid before anything else."
                            );
                            gone.push(hx.clone());
                        }
                        GoneAction::Rebroadcast => {
                            // Re-sign nothing. A fresh transaction for the same
                            // window carries a fresh timestamp and so a different
                            // hash, and replay protection here is by hash alone:
                            // if a peer still holds the first one, BOTH confirm
                            // and the operator pays these miners twice out of its
                            // own wallet. The identical bytes can only be mined
                            // once, whoever mines them.
                            let body = rec.map(|r| r.body_hex.clone()).unwrap_or_default();
                            let resp = post_hex(
                                &client,
                                &format!("{node}/submit/transaction?hexbody=true"),
                                &body,
                            );
                            eprintln!(
                                "[settle] payout tx {short} is not in the node's mempool. These \
                                 bytes were put on the network, so it can still be mined and this \
                                 pool will not re-sign the window. Re-broadcast the identical \
                                 signed bytes (same hash) -> {resp}"
                            );
                            wait.note(PayoutTxState::Gone);
                            still.push(hx.clone());
                        }
                        GoneAction::Stuck => {
                            eprintln!(
                                "[settle] payout tx {short} left the node's mempool. The node held \
                                 it once, so it was relayed and can still be mined, but this pool \
                                 has no stored bytes for it (it predates them) and will NOT re-sign \
                                 the window: that would pay these miners twice if the first one is \
                                 ever included. Settlement is frozen until it confirms or you \
                                 remove it from settle_pending_txs by hand, having checked a block \
                                 explorer that it is really dead."
                            );
                            wait.note(PayoutTxState::Gone);
                            still.push(hx.clone());
                        }
                    }
                }
                PayoutTxState::Pending => {
                    println!(
                        "[settle] payout tx {short} is still waiting in the node's mempool; \
                         nobody is paid until a block includes it"
                    );
                    wait.note(PayoutTxState::Pending);
                    seen_held.push(hx.clone());
                    still.push(hx.clone());
                }
                PayoutTxState::Confirming(d) => {
                    println!(
                        "[settle] payout tx {short} is only {d} block(s) deep; \
                         waiting for {PAYOUT_MATURITY_DEPTH} before settling again"
                    );
                    wait.note(PayoutTxState::Confirming(d));
                    seen_held.push(hx.clone());
                    still.push(hx.clone());
                }
                PayoutTxState::Unknown => {
                    eprintln!(
                        "[settle] could not determine the state of payout tx {short}; \
                         keeping it and skipping this cycle"
                    );
                    wait.note(PayoutTxState::Unknown);
                    still.push(hx.clone());
                }
            }
        }
        // Credit the buried ones and forget the dead ones in ONE locked step, so
        // a unit is never simultaneously in flight and paid, and never neither.
        if !buried.is_empty() || !gone.is_empty() || !seen_held.is_empty() {
            let (shot, credited) = {
                let mut guard = plock(pool);
                // Reborrow so the two ledgers can be handed over together: they
                // MUST move in one step, or a unit is briefly in both or neither.
                let p = &mut *guard;
                let now = curtimes();
                // Written BEFORE anything is dropped or credited, and in the
                // same durable snapshot: this is the fact that stops a later
                // "I do not know that hash" being read as "it was never on the
                // network".
                for hx in &seen_held {
                    if let Some(r) = p.payout_records.iter_mut().find(|r| &r.hash == hx) {
                        r.node_holds = true;
                    }
                }
                let mut credited: Vec<(String, u64, usize)> = Vec::new();
                for hx in &buried {
                    if let Some(rec) = confirm_payout(&mut p.payout_records, &mut p.paid, hx, now) {
                        credited.push((hx.clone(), rec.units(), rec.rows.len()));
                    }
                }
                for hx in &gone {
                    // Nothing was paid, and these rows name the miners it was
                    // for. They become a debt, not free money in the pot: the
                    // pot would be re-split over whoever is mining now.
                    if let Some(rec) = drop_payout(&mut p.payout_records, hx) {
                        owe_rows(&mut p.owed, &rec.rows);
                    }
                }
                p.rebuild_inflight();
                p.settle_pending_txs.retain(|h| still.contains(h));
                (p.state_shot(true), credited)
            };
            flush_state(shot);
            for (hx, units, n) in credited {
                let short = hx.get(..16).unwrap_or(&hx);
                println!(
                    "[settle] payout tx {short} is buried: {units} unit(s) to {n} miner(s) are \
                     now PAID"
                );
            }
        }
        if !still.is_empty() {
            // Some payout is still in flight (or unresolved); keep those and skip.
            let now = curtimes();
            let (shot, since, packed_txs, target_block_secs) = {
                let mut p = plock(pool);
                p.settle_pending_txs = still;
                // Stamped on the FIRST cycle that could not resolve, and left
                // alone afterwards: this is the baseline everything below is
                // measured from. Nothing new is submitted while a payout is
                // outstanding (this branch returns), so it always refers to the
                // set being waited on.
                let since = *p.stall_since.get_or_insert((tip, now));
                (
                    p.state_shot(true),
                    since,
                    // Measured, not assumed: how many of the node's transactions
                    // this pool's own next block would carry. It is the evidence
                    // for the one claim below that is about the pool itself.
                    p.tpl.txs.bodies.len(),
                    p.params.target_time,
                )
            };
            flush_state(shot);
            // A payout that never confirms freezes EVERY later payout, so this
            // has to be said. It also has to be TRUE: measured in blocks since
            // the pool first failed to resolve it (and in seconds, for the chain
            // that has stopped producing blocks entirely), never in settlement
            // cycles, which are an operator setting and made this fire on every
            // healthy payout.
            let blocks_waited = tip.saturating_sub(since.0);
            let secs_waited = now.saturating_sub(since.1);
            if payout_stalled(blocks_waited, secs_waited, target_block_secs) {
                eprintln!(
                    "{}",
                    stall_warning(blocks_waited, secs_waited, &wait, packed_txs)
                );
            }
            return;
        }
        // Every prior payout is buried or definitively gone: clear and settle
        // fresh income.
        let shot = {
            let mut p = plock(pool);
            p.settle_pending_txs.clear();
            // Nothing is outstanding, so the next payout starts its own wait
            // from the tip it is submitted against.
            p.stall_since = None;
            p.state_shot(true)
        };
        flush_state(shot);
    } else {
        // Nothing outstanding at all, so nothing is being waited on. Cleared
        // here too because a payout can leave `settle_pending_txs` by a route
        // this poll never ran: an operator editing the state file by hand, which
        // the frozen-payout message above tells them to do. Left set, the next
        // payout inherits an age it never had and is reported stuck the moment
        // it is submitted - which is the same false alarm this whole check
        // exists to stop.
        plock(pool).stall_since = None;
    }

    // The share target the pool is serving cannot be accounted on, so the credit
    // any split below would be computed from is not a measure of anybody's work.
    // A payout is a signed on-chain transfer: once it is broadcast the money is
    // gone, and no later correction takes it back from the worker that took the
    // window by submitting fastest. So nothing FRESH is settled until the
    // difficulty recovers - the shares themselves already stopped being credited,
    // and what honest miners earned before the fall is still in the window
    // waiting.
    //
    // This stands HERE, and not at the top of the cycle, because the payout
    // resolution above is not a payment: it is what turns a miner's IN FLIGHT
    // into PAID once the node reports the paying transaction buried. Returning
    // before it froze every already-submitted payout for the whole length of the
    // halt - the very thing the comment on that block says it must not do - so a
    // miner's last payout would have read "in flight" until the chain recovered,
    // and a failed chunk's rows would not have reached the owed ledger either.
    // Nothing under here has valued a wallet or signed anything yet.
    let halted = plock(pool).share_halt.clone();
    if let Some(why) = halted {
        eprintln!(
            "[settle] payouts already in flight were resolved, but NOTHING FRESH is being \
             settled: {why}"
        );
        return;
    }

    // What a previous cycle failed to deliver. Read here, after the poll above
    // has had its chance to put a dropped chunk's rows back on it.
    let owed = plock(pool).owed.clone();

    // Nothing credited in the window AND no old debt: there is nothing to pay.
    // An empty window alone is not enough to stop here - a chunk that failed
    // while those miners' shares were in the window is still owed to them long
    // after the window has rolled past.
    if counts.is_empty() && owed.is_empty() {
        return;
    }

    let bal = balance(&client, &node, acc.readable());
    // An answer we cannot value is NOT a zero balance: paying out on a garbled or
    // implausible one would sign transactions for a number the node never
    // reported, and publishing it as a zero tells every miner it is owed nothing.
    // Skip the cycle instead; the accounting is untouched.
    let Some(units) = bal.units() else {
        eprintln!(
            "[settle] the pool cannot value its own wallet ({bal}); \
             skipping this settlement cycle"
        );
        // Say so to miners too: `/earnings` must report PENDING as stale rather
        // than keep quoting a figure the pool can no longer confirm.
        plock(pool).matured_current = false;
        return;
    };
    // Read the hold-back AFTER the balance, never before. A block found in
    // between then appears in the hold-back but not yet in the balance, which
    // errs towards paying LESS; the other order would let a block found during
    // the (slow) pending-payout poll slip through and be paid at 0 confirmations.
    //
    // Counting a block's TRANSACTION FEES is part of reading that hold-back, and
    // it happens here rather than on the confirmation loop's own clock so there
    // is no window in which a block has landed, its fees are in the balance, and
    // this settlement has not looked for them yet.
    let Some(immature_units) = count_immature_fees(pool, &client, &node) else {
        // The pool knows there is income it cannot value. Miners are told their
        // pending figure is STALE rather than paid out of a number that is
        // missing a block's fees.
        plock(pool).matured_current = false;
        return;
    };

    // Keep a reserve so the wallet always covers the (per-chunk) tx fee. No pool
    // fee is skimmed: this is a community pool, and the reserve covers the fees.
    // `/terms` reports this same constant, so what a miner is told is what runs.
    let reserve = SETTLE_RESERVE_UNITS;
    // Hold back the whole income of blocks that are not yet buried - subsidy AND
    // the transaction fees the chain credited with it: distributing income a
    // reorg can still revoke costs the operator money that nothing can claw
    // back, because the payout stays valid on the new chain.
    let distributable = distributable_units(units, immature_units, reserve);
    // Publish the valuation this cycle just made, so a miner polling /earnings
    // right after a settlement sees the same figure the settlement acted on
    // instead of one up to MONEY_REFRESH_CYCLES old. `None` here is a KNOWN
    // nothing (at or below the reserve), not an unreadable balance.
    {
        let mut p = plock(pool);
        p.matured = Some(Matured {
            units: distributable.unwrap_or(0),
            at: curtimes(),
        });
        p.matured_current = true;
    }
    let Some(distributable) = distributable else {
        if immature_units > 0 {
            println!(
                "[settle] holding back {immature_units} unit(s) of block income that is not yet \
                 buried {COINBASE_MATURITY_DEPTH} deep; nothing matured to pay this cycle"
            );
        }
        return;
    };

    // What a failed chunk already owes named miners comes off the top, before a
    // single unit of fresh income is split. Splitting first and paying the debt
    // out of what is left would hand that money to the current window - which
    // includes the miners whose chunks DID go through, so they would be paid
    // twice for the same window and the miners who were paid nothing would fund
    // it.
    let (mut plan, left) = take_owed(&owed, distributable);
    let owed_now: u64 = plan.iter().map(|(_, u)| *u).sum();
    if owed_now > 0 {
        println!(
            "[settle] paying {owed_now} unit(s) owed to {} miner(s) by an earlier settlement that \
             did not reach the node, before splitting anything",
            plan.len()
        );
    }
    // Split what is left over PAYABLE workers only, so IP-fallback / unpayable
    // keys do not dilute the honest miners' proportional share.
    let payable_counts: Vec<(String, u64)> = counts
        .into_iter()
        .filter(|(w, _)| is_payout_address(w))
        .collect();
    plan.extend(plan_settlement(left, &payable_counts));
    // A miner that is owed AND has shares in the window is paid once, in one
    // action: every action counts against the node's TX_ACTIONS_MAX limit that
    // PAYOUT_CHUNK is sized against.
    merge_payout_rows(&mut plan);
    if plan.is_empty() {
        return;
    }

    let main = Address::from(*acc.address());
    let mut tally = SettleTally::default();
    for chunk in plan.chunks(PAYOUT_CHUNK) {
        // 0.01 HAC network fee, funded by the reserve. Built from the same helper
        // `/terms` quotes, so the fee a miner is told about is the fee the
        // transaction carries.
        let mut tx = TransactionType2::new_by(main.clone(), chunk_tx_fee(), curtimes());
        // Exactly what this transaction pays, in the order it pays it. Only rows
        // that made it into the transaction are here: a recipient the pool had to
        // skip must never appear in anyone's accounting as money in flight.
        let mut rows: Vec<(String, u64)> = Vec::with_capacity(chunk.len());
        for (addr, u) in chunk {
            let Ok(to) = Address::from_readable(addr) else {
                continue;
            };
            let mut act = HacToTrs::new();
            act.to = AddrOrPtr::from_addr(to);
            act.hacash = payout_amount(*u);
            if tx.push_action(Box::new(act)).is_err() {
                break; // should not happen within a <=190 chunk, but stay safe
            }
            rows.push((addr.clone(), *u));
        }
        let pushed = rows.len();
        let chunk_units: u64 = rows.iter().map(|(_, u)| *u).sum();
        if pushed == 0 {
            continue;
        }
        if tx.fill_sign(&acc).is_err() {
            eprintln!("[settle] signing failed for a chunk; skipping it");
            continue;
        }
        // Record the payout tx hash BEFORE submitting AND persist it, so a lost
        // ACK or a crash mid-settlement still blocks a second payout: next cycle
        // we poll this hash and only retry if it is gone.
        //
        // The per-recipient rows are written in the SAME snapshot, not after the
        // node answers. A crash in between would otherwise leave a tracked hash
        // with no rows behind it, and when that payout later confirmed the pool
        // could not tell a single miner it had been paid.
        let txhash = hex::encode(tx.hash().serialize());
        // Serialized BEFORE the record is written, because the record carries it:
        // these bytes are the only way to put this exact transaction back on the
        // network if the node loses it, and the only alternative is re-signing
        // the window into a second transaction that can also be mined.
        let body = hex::encode(tx.serialize());
        let shot = {
            let mut p = plock(pool);
            p.settle_pending_txs.push(txhash.clone());
            // What this chunk pays comes off the owed ledger in the SAME locked
            // step and the same snapshot. If the chunk then fails, its rows go
            // back on; if the process dies here, the hash is tracked and the next
            // cycle's poll puts them back. Either way a debt is never both
            // cleared and unpaid.
            deduct_owed(&mut p.owed, &rows);
            p.payout_records.push(PayoutRecord {
                hash: txhash.clone(),
                at: curtimes(),
                // Not yet: the node has not been asked whether it holds this.
                node_holds: false,
                body_hex: body.clone(),
                rows,
            });
            p.rebuild_inflight();
            p.state_shot(true)
        };
        // The fsync happens here, with the pool lock released: no miner request
        // waits on the state disk, and nothing is submitted before it lands.
        let recorded = flush_state(shot);
        if !recorded {
            // An untracked payout is one a later cycle could pay all over again.
            // Nothing was submitted, so stopping here loses nothing; the hash
            // stays in memory and next cycle's poll resolves it as gone.
            eprintln!(
                "[settle] could not record the payout tx on disk; NOT submitting this chunk \
                 (an untracked payout could be paid twice)"
            );
            break;
        }
        let resp = post_hex(
            &client,
            &format!("{node}/submit/transaction?hexbody=true"),
            &body,
        );
        // Surface a node rejection instead of silently reporting success - and
        // never confuse one with a request that got no answer at all.
        let short = &txhash[..txhash.len().min(16)];
        match submit_verdict(&resp) {
            SubmitVerdict::Accepted => {}
            SubmitVerdict::Rejected => {
                eprintln!(
                    "[settle] node did NOT accept payout tx {short} ({pushed} recipients): {resp}"
                );
                // The node's own validator refused it, so it never inserted it
                // and never relayed it. Nothing can pay this: put the rows back
                // on the owed ledger.
                owe_back_failed_payout(pool, &txhash);
                tally.record(ChunkOutcome::Failed, pushed, chunk_units);
                continue;
            }
            SubmitVerdict::Unresolved => {
                // A timeout or a dropped connection is NOT a refusal. The node
                // may have taken these bytes, inserted them and relayed them
                // before the answer was lost, and the money would then really be
                // in flight. Forgetting the hash here is how the next cycle
                // signs a SECOND transaction for the same window and the
                // operator pays twice.
                eprintln!(
                    "[settle] no usable answer submitting payout tx {short} ({pushed} recipients, \
                     {chunk_units} units): {resp}\n\
                     [settle] the node may hold it, so it stays in the pending ledger and is NOT \
                     re-issued; the next cycle asks the node what it has."
                );
                tally.record(ChunkOutcome::Unresolved, pushed, chunk_units);
                continue;
            }
        }
        // ret=0 only means the API took the bytes. The node validates
        // synchronously and then inserts into the mempool on a background task
        // whose result it DISCARDS, so a transaction that fails there is
        // reported as accepted and simply never exists. Ask the node what it
        // actually holds before counting a single unit as sent.
        match verify_admitted(&client, &node, &txhash) {
            Admission::Held => {
                println!(
                    "[settle] submitted payout tx {short} paying {pushed} miner(s) \
                     {chunk_units} units; the node holds it"
                );
                // Upgrade the record: the node confirms it holds this, so every
                // recipient can be told its money is really in flight rather than
                // merely submitted. Still NOT paid - that needs burial.
                let shot = {
                    let mut p = plock(pool);
                    if let Some(r) = p.payout_records.iter_mut().find(|r| r.hash == txhash) {
                        r.node_holds = true;
                    }
                    p.state_shot(true)
                };
                flush_state(shot);
                tally.record(ChunkOutcome::Delivered, pushed, chunk_units);
            }
            Admission::Missing => {
                eprintln!(
                    "[settle] payout tx {short} was accepted by the API but the node does NOT \
                     hold it ({pushed} recipients, {chunk_units} units): nothing was paid and \
                     nothing was relayed. These rows are now OWED and the next cycle pays them \
                     before it splits anything else."
                );
                owe_back_failed_payout(pool, &txhash);
                tally.record(ChunkOutcome::Failed, pushed, chunk_units);
            }
            Admission::Unresolved => {
                eprintln!(
                    "[settle] could not confirm the node holds payout tx {short} \
                     ({pushed} recipients, {chunk_units} units); keeping it in the pending \
                     ledger and NOT counting it as paid"
                );
                tally.record(ChunkOutcome::Unresolved, pushed, chunk_units);
            }
        }
    }
    if !tally.attempted() {
        return;
    }
    // Loud on anything that did not reach the node: an operator reading only the
    // closing line must never be told money moved when it did not.
    if tally.all_delivered() {
        println!("{}", tally.summary());
    } else {
        eprintln!("{}", tally.summary());
    }
}

/// Wraps the share-credit path: takes the pool lock only twice (a brief snapshot
/// and a brief commit), computing the expensive x16rs hash OFF the lock so one
/// submission cannot serialize every other miner behind a full PoW evaluation.
/// Replay protection stays atomic: the (height, coinbase_nonce, block_nonce) key
/// is inserted under the commit lock and a second submission of the same key is
/// rejected there.
fn handle_submission(
    pool: &Arc<Mutex<Pool>>,
    worker: &str,
    height: u64,
    coinbase_nonce: [u8; 32],
    block_nonce: u32,
) -> serde_json::Value {
    // No route may seat an unpayable key in the PPLNS window. The window is a
    // fixed 4096 shares shared by everyone, so a key that is filtered out at
    // payout still evicts shares from miners the pool CAN pay - their work is
    // credited for a shorter stretch and small/sporadic miners fall out of the
    // window before a block is found. The paid path already refuses these; this
    // is the backstop for every other caller.
    if !is_payout_address(worker) {
        return json!({
            "ok": false,
            "kind": "invalid",
            "err": "set worker=<your HAC address> so the pool can pay you"
        });
    }
    let key = (height, coinbase_nonce, block_nonce);
    // Phase 1 - brief lock: reject stale/duplicate early and snapshot the inputs.
    let (tpl, share_target, network_target, client, node) = {
        let p = plock(pool);
        if height != p.tpl.height {
            return json!({"ok":false,"kind":"stale","height":p.tpl.height});
        }
        if p.seen.contains(&key) {
            return json!({"ok":false,"kind":"duplicate"});
        }
        (
            p.tpl.clone(),
            p.share_target,
            p.network_target,
            p.client.clone(),
            p.node.clone(),
        )
    };

    // Phase 2 - no lock: rebuild exactly what the worker hashed and evaluate the
    // (deliberately slow) x16rs PoW hash without blocking any other request.
    let cb = coinbase_with_extranonce(&tpl, &coinbase_nonce);
    let intro = intro_bytes(&tpl, &cb, block_nonce);
    let hash = pool_core::hash_of(tpl.height, &intro);
    if !pool_core::beats(&hash, &share_target) {
        // The pool never trusts a worker's own header: it rebuilds one from
        // (height, coinbase_nonce, block_nonce) and hashes THAT, so a worker
        // that computes a different merkle root has its shares rejected rather
        // than credited. It cannot steal - but it also cannot earn, and a silent
        // reject counter is how a miner burns a day of hashrate for nothing.
        //
        // The message is built under the lock (it reads when the header last
        // changed) and printed off it: stderr can block on a slow console, and
        // every other miner's request is serialized behind this same mutex.
        let now_ms = pool_core::now_ms();
        let note = plock(pool).note_bad_share(worker, now_ms);
        if let Some(msg) = note {
            eprintln!("{msg}");
        }
        return json!({"ok":false,"kind":"invalid","err":"above share target"});
    }
    let is_block = pool_core::beats(&hash, &network_target);

    // Phase 3 - brief lock: atomically re-check freshness + replay, then credit.
    // The accounting snapshot leaves the lock as bytes; writing it is phase 3b,
    // because every other request is serialized behind this same mutex and a
    // create/rename/fsync must never happen underneath it.
    let (commit, shot) = {
        let mut p = plock(pool);
        if height != p.tpl.height {
            return json!({"ok":false,"kind":"stale","height":p.tpl.height});
        }
        // The share target in force cannot be accounted on: a share is no longer
        // evidence of work, so crediting one would hand a slice of the next
        // settlement - real money, unrecoverable once sent - to whoever submits
        // fastest rather than to whoever hashed. Refuse with the reason, so a
        // miner sees why its shares stopped counting instead of mining all day
        // into a window it is being paid nothing from.
        //
        // A solution that beats the NETWORK target is a whole block and is never
        // dropped for this: it cost a block's work whatever the share target says,
        // and throwing it away would cost the pool the entire reward.
        if !is_block {
            if let Some(why) = &p.share_halt {
                return json!({"ok": false, "kind": "degraded", "err": why});
            }
        }
        // The rate limiter exists to bound the replay set, not to throw money
        // away: a submission that beats the NETWORK target is a whole block
        // reward, so it is evaluated FIRST and is never refused here. Only
        // ordinary shares are shed at the cap.
        if share_limiter_rejects(p.seen.len(), is_block) {
            return json!({"ok":false,"kind":"busy","err":"too many shares this height"});
        }
        // Same rule for the per-worker budget: a solution that beats the NETWORK
        // target is a whole block reward and is never refused for arriving fast.
        let at_ms = pool_core::now_ms();
        if !is_block && !p.rate_admits_share(worker, at_ms) {
            return json!({
                "ok": false,
                "kind": "throttled",
                "err": "submitting faster than this share target says any hardware could find \
                        shares; slow down or ask the operator for an easier share_bits"
            });
        }
        if !p.seen.insert(key) {
            return json!({"ok":false,"kind":"duplicate"});
        }
        // Stamped with the pool's own clock at acceptance. This is what makes a
        // withheld batch worthless: credit is how long a share has been in the
        // window, so a share dumped at the settlement tick has earned nothing.
        p.pplns.record(worker, at_ms);
        p.note_good_share(worker);
        p.accepted += 1;
        if !is_block {
            // Composed under the lock (it reads the running count and the
            // workers behind it) and printed off it, for the same reason the
            // above-target line is: this is the share hot path.
            let line = p.share_log.note(worker, height, at_ms);
            let accepted = p.accepted;
            (Commit::Share { accepted, line }, p.note_share_saved())
        } else {
            // Serializing the block is deliberately NOT done here: it now copies
            // the node's whole transaction set, and every other miner's request
            // is serialized behind this mutex. It needs nothing but the phase-1
            // snapshot, so phase 4 builds it with the lock released.
            let solved = tpl.height;
            p.submitted.push((solved, hash)); // counted once the bg thread sees it stick
            // Hold this block's income back from settlement until the chain has
            // buried it. The node credits the reward the moment the block is
            // inserted, so without this the very next settle tick would pay out
            // income that is 0-1 confirmations deep.
            //
            // The subsidy is only half of it. The chain also credits this
            // address the sum of the fees of every transaction in the block, and
            // the pool packs the node's transactions, so a found block brings in
            // MORE than `block_reward_units`. That figure is not knowable here -
            // the packed transactions are opaque bytes and /submit/block does
            // not report it - so it starts uncounted and settlement reads it
            // back off the node before it values anything.
            p.immature.push(Immature {
                height: solved,
                hash,
                units: block_reward_units(solved),
                fees_counted: false,
            });
            (
                Commit::Block(block_found_line(worker, solved, &hash)),
                p.state_shot(true),
            )
        }
    };

    // Phase 3b - no lock: persist the accounting (fsync on a block) before the
    // block goes out, so a crash right after submitting still knows about it.
    flush_state(shot);

    match commit {
        Commit::Share { accepted, line } => {
            // At most one of these per SHARE_LOG_EVERY_MS. The pool used to
            // print one per accepted share, and at the minimum share_bits on an
            // easy chain that is thousands a second - which is what made the
            // block notice below unreadable.
            if let Some(line) = line {
                println!("{line}");
            }
            return json!({"ok":true,"kind":"share","accepted":accepted});
        }
        // Never summarised, never suppressed, and printed BEFORE the submit that
        // may hang or be refused: this is the line an operator must not miss.
        Commit::Block(notice) => println!("{notice}"),
    }

    // Phase 4 - no lock: serialize and submit the winning block. This is where
    // the node's packed transactions are carried into the block: OUR coinbase in
    // slot 0, then every transaction the node packed for this height, with a
    // merkle root folded from the node's own sibling list.
    let block_bytes = assemble_block(&tpl, &cb, block_nonce);
    let packed = tpl.txs.bodies.len();
    let submit = submit_block_bytes(&client, &node, &block_bytes);
    // The submit answer used to go only into the JSON the winning worker reads,
    // so an outright refusal - a whole block reward - never reached the operator.
    if block_submit_refused(&submit) {
        eprintln!(
            "[block] the node REFUSED our block at height {height} ({packed} packed tx(s), {} \
             bytes): {submit}. That block's entire reward is lost.",
            block_bytes.len()
        );
    } else {
        println!(
            "[block] submitted height {height} carrying {packed} packed tx(s) ({} bytes): \
             {submit}",
            block_bytes.len()
        );
    }
    json!({"ok":true,"kind":"block","solved_height":height,"submit":submit})
}

/// Did `/submit/block` refuse the block outright?
///
/// The node validates asynchronously, so `ret:0` means only "parsed and queued"
/// and is NOT proof of acceptance - `note_block_stalls` is what catches a later
/// silent refusal. But `ret:1`, a transport failure, or an unparseable answer
/// are definitive, and each one costs a whole block reward.
fn block_submit_refused(resp: &str) -> bool {
    match serde_json::from_str::<serde_json::Value>(resp) {
        Ok(j) => find_u64(&j, "ret") != Some(0),
        Err(_) => true,
    }
}

/// Should the per-height share rate limiter refuse this submission?
///
/// The cap bounds the replay set for one height. Applying it before the block
/// check meant that at the cap the pool answered "busy" to a solution that beat
/// the NETWORK target and threw away a whole block reward - the most valuable
/// thing a miner can hand it. A network block is one entry, is vanishingly rare
/// next to the cap, and is worth far more than the memory it costs, so it is
/// always admitted.
fn share_limiter_rejects(seen_len: usize, is_block: bool) -> bool {
    !is_block && seen_len >= SEEN_CAP
}

/// What phase 3 of a submission decided, carried out of the lock scope so the
/// answer is built (and the state written, and the block serialized) with the
/// pool mutex released.
enum Commit {
    Share {
        accepted: u64,
        /// The summary line this share fell due for, if any: `None` means it was
        /// folded into a line another share will print.
        line: Option<String>,
    },
    /// A whole block, and the line announcing it. Carried as a string so it is
    /// printed with the pool lock released, like everything else on this path.
    Block(String),
}

fn hash32(s: &str) -> Option<[u8; 32]> {
    let v = hex::decode(s).ok()?;
    if v.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Some(out)
}

fn parse32(s: Option<&String>) -> Option<[u8; 32]> {
    hash32(s?)
}

/// Read the HTTP request line under BOTH a size cap and an ABSOLUTE wall-clock
/// deadline.
///
/// A socket read timeout bounds each read syscall, not the request: a client
/// dribbling one byte every few seconds resets the timer on every byte and can
/// hold a handler thread (plus its global and per-IP slot) for hours, which is
/// all a slow-loris needs to starve every real miner. Checking elapsed time
/// after each read is what actually bounds it.
fn read_request_line(s: TcpStream) -> Option<String> {
    let start = Instant::now();
    let mut reader = BufReader::new(s.take(MAX_REQUEST_LINE as u64));
    let mut line: Vec<u8> = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        if start.elapsed() >= REQUEST_READ_DEADLINE {
            return None;
        }
        match reader.read(&mut byte) {
            Ok(0) => return None, // peer closed before sending a full line
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                if line.len() >= MAX_REQUEST_LINE {
                    return None;
                }
                line.push(byte[0]);
            }
            // A per-read timeout is not fatal on its own; the deadline above is
            // what ends the connection. Any other error is.
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return None,
        }
    }
    String::from_utf8(line).ok()
}

fn handle(mut s: TcpStream, pool: Arc<Mutex<Pool>>) {
    // Bound how long a client may hold a connection and how much we read, so a
    // slow-loris or a socket that never sends a newline cannot pin a thread or
    // grow memory without limit. The request line we care about is tiny. The
    // per-read timeout is short so the absolute deadline is honoured promptly.
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
    let Ok(peek) = s.try_clone() else { return };
    let Some(line) = read_request_line(peek) else {
        return;
    };
    let target = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let params = parse_query(&query);
    // The standard miner API carries no worker id, so attribute by source IP.
    let peer = s
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let body = route(&path, &params, &pool, &peer);
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = s.write_all(resp.as_bytes());
    let _ = s.flush();
    // Orderly close: signal end-of-write, then drain any unread request bytes so
    // the peer's pending data does not RST-truncate our response (seen on Windows).
    let _ = s.shutdown(Shutdown::Write);
    let mut sink = [0u8; 2048];
    let _ = s.read(&mut sink);
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn route(
    path: &str,
    params: &HashMap<String, String>,
    pool: &Arc<Mutex<Pool>>,
    peer: &str,
) -> String {
    match path {
        // ---- standard Hacash miner API: an UNMODIFIED poworker mines here ----
        "/query/miner/pending" => plock(pool).pending_cache.clone(),
        "/query/miner/notice" => {
            let want: u64 = params
                .get("height")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let wait: u64 = params
                .get("wait")
                .and_then(|v| v.parse().ok())
                .unwrap_or(45)
                .clamp(1, 120);
            // Budget long-polls separately: if too many are already parked, answer
            // immediately with the current height rather than holding another slot.
            if NOTICE_WAITERS.fetch_add(1, Relaxed) >= MAX_NOTICE_WAITERS {
                NOTICE_WAITERS.fetch_sub(1, Relaxed);
                let h = plock(pool).tpl.height;
                return json!({"ret":0,"height":h}).to_string();
            }
            let _ng = NoticeGuard;
            let deadline = Instant::now() + Duration::from_secs(wait);
            loop {
                let h = plock(pool).tpl.height; // brief lock only
                if h > want || Instant::now() >= deadline {
                    return json!({"ret":0,"height":h}).to_string();
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        }
        "/submit/miner/success" => {
            let height: u64 = params
                .get("height")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let block_nonce: u32 = params
                .get("block_nonce")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let Some(cn) = parse32(params.get("coinbase_nonce")) else {
                return json!({"ret":1,"err":"bad coinbase_nonce"}).to_string();
            };
            // A share is only worth crediting if we can pay it. Require the miner
            // to announce a payable address; crediting an IP-fallback key that is
            // then dropped at payout would silently mine for nothing.
            let Some(worker) = params
                .get("worker")
                .filter(|w| is_payout_address(w))
                .cloned()
            else {
                return json!({
                    "ret": 1,
                    "err": "set pool_worker=<your HAC address> so the pool can pay you"
                })
                .to_string();
            };
            let _ = peer; // no longer used for attribution on the paid path
            let r = handle_submission(pool, &worker, height, cn, block_nonce);
            let ok = r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            let kind = r.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            // Nothing is printed here any more. This route used to print one
            // line per accepted share - 7.6 MB in four minutes on a rig at the
            // pool's own minimum share_bits - and the block-found notice went
            // out through that SAME println, so the one line that matters was
            // buried in it. `handle_submission` now reports both, on the paid
            // path and on /share alike: blocks always, shares as one summary
            // line per SHARE_LOG_EVERY_MS.
            if ok {
                json!({"ret":0,"kind":kind}).to_string()
            } else {
                json!({"ret":1,"kind":kind}).to_string()
            }
        }

        // ---- our own simple protocol (hbit-test-miner) ----
        // Both routes demand a payable worker exactly like the paid path: a share
        // credited to a key the pool cannot pay is work done for nothing that
        // also evicts a payable miner's share from the shared 4096-share window.
        "/work" => {
            let Some(worker) = params
                .get("worker")
                .filter(|w| is_payout_address(w))
                .cloned()
            else {
                return json!({
                    "ok": false,
                    "err": "set worker=<your HAC address> so the pool can pay you"
                })
                .to_string();
            };
            let mut p = plock(pool);
            let en = p.extranonce_for(&worker);
            let cb = coinbase_with_extranonce(&p.tpl, &en);
            let intro = intro_bytes(&p.tpl, &cb, 0);
            json!({
                "ok": true,
                "height": p.tpl.height,
                "intro": hex::encode(intro),
                "share_target": hex::encode(p.share_target),
                "network_target": hex::encode(p.network_target),
                "extranonce": hex::encode(en),
            })
            .to_string()
        }
        "/share" => {
            let Some(worker) = params
                .get("worker")
                .filter(|w| is_payout_address(w))
                .cloned()
            else {
                return json!({
                    "ok": false,
                    "kind": "invalid",
                    "err": "set worker=<your HAC address> so the pool can pay you"
                })
                .to_string();
            };
            let height: u64 = params
                .get("height")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let nonce: u32 = params
                .get("nonce")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let en = {
                let p = plock(pool);
                match p.workers.get(&worker).copied() {
                    Some(en) => en,
                    None => {
                        return json!({"ok":false,"kind":"invalid","err":"unknown worker"})
                            .to_string();
                    }
                }
            };
            handle_submission(pool, &worker, height, en, nonce).to_string()
        }
        "/stats" => {
            // Copy the numbers out, then RELEASE the lock before building the
            // body. /stats is open and unauthenticated, and serializing up to
            // PPLNS_WINDOW worker rows under the global mutex would let anyone
            // stall every miner's /work, /share and /submit by polling it.
            let (height, difficulty, accepted, blocks, pending, orphaned, window, workers, credit) = {
                let p = plock(pool);
                (
                    p.tpl.height,
                    p.tpl.difficulty,
                    p.accepted,
                    p.blocks,
                    p.submitted.len(),
                    p.orphaned,
                    p.pplns.total(),
                    p.pplns.counts(),
                    p.pplns.credit(pool_core::now_ms()),
                )
            };
            json!({
                "height": height,
                "difficulty": difficulty,
                "accepted_shares": accepted,
                "blocks_confirmed": blocks,
                "blocks_pending": pending,
                "blocks_orphaned": orphaned,
                "share_window": window,
                "workers": workers,
                // What a settlement would actually split over. `workers` is the
                // raw headcount and is for looking at only: paying by it is what
                // lets a miner take the whole window with one burst of withheld
                // shares. hbit-pool-payout reads THIS.
                "credit": credit,
                "credit_note": "milliseconds of share residence: how long each worker's shares \
                                have been in the payout window. Payouts are split by this, not by \
                                the `workers` headcount.",
            })
            .to_string()
        }

        // The pool's terms, READ OUT OF the code that enforces them. Nothing here
        // is a number somebody typed into a description: change what the pool
        // does and this changes with it, which is the whole point.
        "/terms" => {
            let (
                window_size,
                horizon_ms,
                share_factor,
                achieved,
                cost_bits,
                halt,
                difficulty,
                settle_secs,
            ) = {
                let p = plock(pool);
                (
                    p.pplns.window() as u64,
                    p.pplns.horizon_ms(),
                    p.share_factor,
                    p.share_factor_achieved,
                    p.share_cost_bits,
                    p.share_halt.clone(),
                    p.tpl.difficulty,
                    p.settle_secs,
                )
            };
            terms_json(
                window_size,
                horizon_ms,
                share_factor,
                achieved,
                cost_bits,
                halt.as_deref(),
                difficulty,
                settle_secs,
                COINBASE_MATURITY_DEPTH,
                PAYOUT_MATURITY_DEPTH,
            )
            .to_string()
        }

        // What ONE worker is owed, what is in flight for it, and what it has been
        // paid. Polled by miners, so it does no node call and holds the pool lock
        // only long enough to copy this worker's own numbers out.
        "/earnings" => {
            let Some(worker) = params.get("worker") else {
                return json!({
                    "ok": false,
                    "kind": "missing_worker",
                    "err": "ask for one worker: /earnings?worker=<your HAC address>",
                })
                .to_string();
            };
            match classify_worker_id(worker) {
                WorkerId::NotAnAddress => json!({
                    "ok": false,
                    "kind": "invalid_address",
                    "worker": worker,
                    "err": "that is not a Hacash address",
                })
                .to_string(),
                WorkerId::Unpayable => json!({
                    "ok": false,
                    "kind": "unpayable_address",
                    "worker": worker,
                    "err": "that is a Hacash address this pool cannot pay, so it never credits \
                            shares to it. Use a normal single-key address.",
                })
                .to_string(),
                WorkerId::Payable => {
                    let e = plock(pool).earnings_of(worker, pool_core::now_ms());
                    earnings_json(worker, &e, curtimes()).to_string()
                }
            }
        }
        _ => json!({"ok":false,"err":"no such endpoint"}).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use basis::difficulty::LOWEST_DIFFICULTY;
    use hbit_pool::{PackedTxs, PaidRow, admission_of};
    use std::sync::Arc;

    /* ---- the money a miner is shown: paid, in flight, pending ---- */

    /// Two real, payable mainnet-format addresses. The pool refuses a share from
    /// anything else, so the accounting keys are always addresses like these.
    const W_A: &str = "1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9";
    const W_B: &str = "12vi7DEZjh6KrK5PVmmqSgvuJPCsZMmpfi";

    fn a_payout(hash: &str, at: u64, node_holds: bool, rows: &[(&str, u64)]) -> PayoutRecord {
        PayoutRecord {
            hash: hash.to_string(),
            at,
            node_holds,
            // Stand-in for the signed bytes. A record the node held must always
            // carry them, or the pool has nothing to re-broadcast.
            body_hex: format!("00{hash}"),
            rows: rows.iter().map(|(w, u)| (w.to_string(), *u)).collect(),
        }
    }

    #[test]
    fn a_share_save_never_stands_in_for_the_settlement_fsync() {
        // What this costs when it goes wrong: the settlement thread writes the
        // hash and rows of a payout it has just SIGNED, asks for that to be
        // fsynced, and is told yes when nothing was fsynced - the share hot path
        // had already replaced the file with its own 16-share save, which skips
        // the fsync on purpose. The payout is broadcast on that yes. Pull the
        // power in the seconds after and the pool comes back with no record of
        // the transaction it signed, so the next cycle signs a second payout for
        // the same PPLNS window and the operator funds it. Nothing unusual is
        // needed to reach it: the pool mutex is not fair, so the share path only
        // has to reach the disk first.
        //
        // NOTE: PERSIST is process-global, so this must stay the only test that
        // calls flush_state.
        let mut path = std::env::temp_dir();
        path.push(format!("hbit-pool-flush-{}.state.json", std::process::id()));
        let path = path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&path);
        *PERSIST.lock().unwrap_or_else(|e| e.into_inner()) = Persisted::default();
        let shot = |seq: u64, durable: bool, body: &str| {
            Some(StateShot {
                seq,
                path: path.clone(),
                body: body.as_bytes().to_vec(),
                durable,
            })
        };
        let on_disk = || std::fs::read_to_string(&path).expect("state file");

        // The settlement took its snapshot under the pool lock first (seq 7); the
        // share path took the next one (seq 8) and reached the disk first.
        assert!(flush_state(shot(8, false, "shares")));
        assert_eq!(on_disk(), "shares");
        // Now the settlement's snapshot arrives. It is the one that was asked to
        // be durable and nothing durable has been written at all, so it has to be
        // written. Before this fix it returned true here with the file untouched.
        assert!(flush_state(shot(7, true, "payout")));
        assert_eq!(
            on_disk(),
            "payout",
            "a durable snapshot was reported saved but never written"
        );

        // A durable snapshot that really did land still short-circuits everything
        // behind it, durable or not: that is what keeps these writes off the hot
        // path when the work is already done.
        assert!(flush_state(shot(6, true, "older payout")));
        assert!(flush_state(shot(5, false, "older shares")));
        assert_eq!(on_disk(), "payout");

        // A later share save proceeds as before - and does NOT make the file
        // count as durable, so the settlement after it is still written.
        assert!(flush_state(shot(9, false, "more shares")));
        assert_eq!(on_disk(), "more shares");
        assert!(flush_state(shot(10, true, "second payout")));
        assert_eq!(on_disk(), "second payout");

        // Nothing to write is not a failed write: the caller may proceed.
        assert!(flush_state(None));
        let _ = std::fs::remove_file(&path);
    }

    fn inflight_total(records: &[PayoutRecord]) -> u64 {
        records.iter().map(|r| r.units()).sum()
    }

    #[test]
    fn a_node_that_is_not_answering_never_publishes_a_zero_pending_pot() {
        // What this costs when it is wrong: the pool publishes matured = 0 with
        // matured_current = true, so /earnings answers every miner "you are owed
        // nothing" in the pool's own voice, and goes on doing it every 30 seconds
        // for the whole outage. Miners whose shares age out of the PPLNS window
        // meanwhile are never paid for that work. It was one line: an unreachable
        // node produced no balance string, and no balance string valued as zero.

        // No answer at all, and the node answering with something that is not a
        // balance. Neither may reach a miner as a figure.
        let down = BalanceAnswer::NoAnswer("connection refused".into());
        let refused = BalanceAnswer::Refused(r#"{"ret":1,"errmsg":"internal"}"#.into());
        assert_eq!(refreshed_money(&down, 0), None);
        assert_eq!(refreshed_money(&refused, 0), None);
        // This is the line template_cycle runs on the result: `None` leaves the
        // last figure the pool could stand behind in place and marks it STALE.
        assert!(!refreshed_money(&down, 0).is_some());

        // A wallet the node really reports as empty is a DIFFERENT answer, and
        // the pool must keep standing behind it. Refusing "0:0" as unreadable
        // would freeze settlement on any pool that has just started.
        assert_eq!(
            refreshed_money(&BalanceAnswer::Reported("0:0".into()), 0),
            Some(0)
        );

        // A funded wallet values exactly as it did before: 1 HAC = 10 units, 2 of
        // them from a block not yet buried, 5 held back as the fee reserve.
        let funded = BalanceAnswer::Reported("1:248".into());
        assert_eq!(refreshed_money(&funded, 2), Some(3));
        assert_eq!(SETTLE_RESERVE_UNITS, 5); // the 5 the 3 above is net of
        // Below the reserve there is nothing to pay, and that is a KNOWN
        // nothing: a real zero the pool publishes as current.
        assert_eq!(refreshed_money(&funded, 8), Some(0));
    }

    #[test]
    fn the_three_money_buckets_never_double_count() {
        // Follow ONE worker's 40 units through the whole lifecycle. At every step
        // the three buckets must add up to the same 40, and no unit may ever be
        // in two of them: that is the entire promise /earnings makes.
        let mut records: Vec<PayoutRecord> = Vec::new();
        let mut paid = PaidLedger::started(1_000);
        // What the pool's wallet says has matured and is unsettled. A payout
        // sitting in the mempool does NOT reduce the node's confirmed balance, so
        // this number keeps containing money that is already in flight - which is
        // exactly why pending has to subtract it.
        let mut matured = 40u64;

        let buckets = |matured: u64, records: &[PayoutRecord], paid: &PaidLedger| {
            let inflight = inflight_total(records);
            let pending = matured.saturating_sub(inflight);
            let done = paid.get(W_A).map(|r| r.units).unwrap_or(0);
            (pending, inflight, done)
        };

        // 1) Nothing submitted yet: it is all pending.
        assert_eq!(buckets(matured, &records, &paid), (40, 0, 0));

        // 2) Submitted. The same 40 units are in flight, and pending must drop to
        // zero even though the confirmed balance still holds them.
        records.push(a_payout("tx1", 1_100, true, &[(W_A, 40)]));
        assert_eq!(buckets(matured, &records, &paid), (0, 40, 0));

        // 3) Buried. It leaves in flight at the same instant it enters paid, and
        // the chain has really spent it so the balance falls with it.
        let rec = confirm_payout(&mut records, &mut paid, "tx1", 1_200).expect("credited");
        assert_eq!(rec.units(), 40);
        matured -= 40;
        assert_eq!(buckets(matured, &records, &paid), (0, 0, 40));

        // Every step totalled 40. Nothing was created and nothing was lost.
        for (p, i, d) in [(40, 0, 0), (0, 40, 0), (0, 0, 40)] {
            assert_eq!(p + i + d, 40);
        }
    }

    #[test]
    fn a_payout_the_node_never_took_goes_back_to_pending_and_never_to_paid() {
        let mut records = vec![a_payout("dead", 1_000, false, &[(W_A, 25)])];
        let mut paid = PaidLedger::started(1_000);
        let matured = 25u64;
        assert_eq!(matured.saturating_sub(inflight_total(&records)), 0);

        // The node definitively does not hold it: nothing was paid and nothing
        // was relayed, so those units are owed again.
        assert!(drop_payout(&mut records, "dead").is_some());
        assert_eq!(inflight_total(&records), 0);
        assert_eq!(matured.saturating_sub(inflight_total(&records)), 25);
        assert!(paid.get(W_A).is_none(), "a dropped payout pays nobody");
        // And it cannot be confirmed after the fact: there is nothing to confirm.
        assert!(confirm_payout(&mut records, &mut paid, "dead", 2_000).is_none());
        assert!(paid.get(W_A).is_none());
    }

    #[test]
    fn a_payout_the_node_relayed_and_then_forgot_is_re_broadcast_never_re_issued() {
        // The failure this prevents: the operator restarts the node. The mempool
        // lives in memory, so it comes back empty, and /query/transaction answers
        // "I do not know that hash" for a payout the node had already validated,
        // accepted and RELAYED to its peers. Reading that as "nothing was paid"
        // makes the pool re-split the same window and sign a second transaction
        // with a fresh timestamp - a different hash, and replay protection here is
        // by hash alone. A peer mines the first, the pool's node mines the second,
        // and the operator has paid those miners twice out of its own wallet with
        // nothing to claw back.
        let not_found: serde_json::Value =
            serde_json::from_str(r#"{"ret":1,"err":"transaction not found"}"#).unwrap();
        assert_eq!(
            classify_payout_tx(&not_found),
            PayoutTxState::Gone,
            "the node's answer is identical whether it never took the tx or lost it"
        );

        // Held once: it WAS relayed. Put the identical bytes back on the network
        // and keep tracking the hash; the same transaction can only be mined once.
        let relayed = a_payout("bb", 1_000, true, &[(W_A, 25)]);
        assert_eq!(gone_action(Some(&relayed)), GoneAction::Rebroadcast);
        assert!(
            !relayed.body_hex.is_empty(),
            "there is nothing to re-broadcast without the signed bytes"
        );

        // A RECORD WITH BYTES BUT NO SIGHTING is the case that still paid twice.
        // `node_holds` is set in exactly one place - after a submit whose
        // verification came back `Held` - so it is false for the two outcomes
        // where relay cannot be ruled out: a submit that timed out, and a
        // verification that could not be read. In both the bytes were on the
        // wire. Reading "no proof of relay" as "never relayed" put the rows back
        // on the owed ledger, and the next cycle signed a second transaction for
        // the same window with a fresh timestamp - a different hash, and both
        // could be mined. The bytes are the test, not the sighting.
        let submitted_through_a_timeout = a_payout("aa", 1_000, false, &[(W_A, 25)]);
        assert!(!submitted_through_a_timeout.node_holds);
        assert_eq!(
            gone_action(Some(&submitted_through_a_timeout)),
            GoneAction::Rebroadcast,
            "a payout whose fate is unknown must go back on the wire, never be re-signed"
        );

        // Held once, but written by a build that kept no bytes: it still must not
        // be re-issued. A frozen payout the operator can see beats a duplicate one
        // nobody can take back.
        let legacy = PayoutRecord {
            body_hex: String::new(),
            ..a_payout("cc", 1_000, true, &[(W_A, 25)])
        };
        assert_eq!(gone_action(Some(&legacy)), GoneAction::Stuck);

        // Neither bytes nor a sighting: nothing could have reached the network,
        // so this is the one shape that is safe to forget and owe again.
        let nothing_on_the_wire = PayoutRecord {
            body_hex: String::new(),
            ..a_payout("dd", 1_000, false, &[(W_A, 25)])
        };
        assert_eq!(gone_action(Some(&nothing_on_the_wire)), GoneAction::Forget);

        // A tracked hash with no record behind it has no bytes and no rows, so
        // there is nothing to re-broadcast and nothing to owe anyone.
        assert_eq!(gone_action(None), GoneAction::Forget);

        // And the bytes survive the state file, which is the only place they are
        // between one settlement cycle and the next.
        let back = PayoutRecord::from_json(&relayed.to_json()).expect("parsed");
        assert_eq!(back.body_hex, relayed.body_hex);
        assert_eq!(back, relayed);
    }

    #[test]
    fn a_submit_that_never_got_an_answer_is_not_a_rejection() {
        // post_hex returns the plain string "http_error: ..." when the request
        // times out. The old test was `ret == 0`, so a timeout read as "the node
        // said no", the pool dropped the hash, and the next cycle signed the same
        // window again - while the node may well have taken the first transaction
        // and relayed it before the answer was lost. Both confirm; the operator
        // pays twice.
        let timeout = "http_error: operation timed out";
        let old_test_said_accepted = serde_json::from_str::<serde_json::Value>(timeout)
            .ok()
            .and_then(|v| find_u64(&v, "ret"))
            == Some(0);
        assert!(
            !old_test_said_accepted,
            "the old test cannot tell a timeout from a refusal"
        );
        assert_eq!(submit_verdict(timeout), SubmitVerdict::Unresolved);

        // The node's own refusal IS definitive: it validates synchronously and
        // only inserts (and relays) after that passes.
        assert_eq!(
            submit_verdict(r#"{"ret":1,"errmsg":"balance not enough"}"#),
            SubmitVerdict::Rejected
        );
        assert_eq!(submit_verdict(r#"{"ret":0}"#), SubmitVerdict::Accepted);
        // Anything else is no verdict at all, and must keep the payout tracked.
        assert_eq!(
            submit_verdict("<html>502 Bad Gateway</html>"),
            SubmitVerdict::Unresolved
        );
        assert_eq!(submit_verdict(""), SubmitVerdict::Unresolved);
        assert_eq!(
            submit_verdict(r#"{"http_error":"connection refused"}"#),
            SubmitVerdict::Unresolved
        );
    }

    #[test]
    fn a_failed_chunk_is_owed_to_its_own_miners_and_paid_before_anything_fresh() {
        // The failure this prevents: two miners with equal shares, 100 units to
        // settle, split into two chunks. A's chunk reaches the node. B's chunk is
        // rejected. The old code threw B's rows away and told the operator "the
        // next cycle re-issues them" - but the next cycle recomputed from the live
        // window and re-split the whole remaining balance over BOTH miners. A, who
        // was already paid in full for that window, took half of B's money.
        let counts = vec![(W_A.to_string(), 1u64), (W_B.to_string(), 1u64)];
        let first = plan_settlement(100, &counts);
        assert_eq!(first.iter().map(|(_, u)| *u).sum::<u64>(), 100);
        let owed_to_b = first
            .iter()
            .find(|(w, _)| w == W_B)
            .cloned()
            .expect("B is in the split");
        assert_eq!(owed_to_b.1, 50);

        // B's chunk failed: its rows become a debt to B, not money in the pot.
        let mut owed: Vec<(String, u64)> = Vec::new();
        owe_rows(&mut owed, &[owed_to_b.clone()]);
        assert_eq!(owed, vec![(W_B.to_string(), 50)]);

        // Next cycle. A's 50 really left the wallet, so 50 is distributable and
        // the window is unchanged.
        let re_split_the_old_way = plan_settlement(50, &counts);
        assert_eq!(
            re_split_the_old_way
                .iter()
                .find(|(w, _)| w == W_A)
                .map(|(_, u)| *u),
            Some(25),
            "the bug: the miner that was already paid takes half of the debt"
        );

        // With the owed ledger, B is made whole first and A gets nothing extra.
        let (mut plan, left) = take_owed(&owed, 50);
        assert_eq!(left, 0, "the debt comes off the top");
        plan.extend(plan_settlement(left, &counts));
        merge_payout_rows(&mut plan);
        assert_eq!(plan, vec![(W_B.to_string(), 50)]);
        assert!(
            !plan.iter().any(|(w, _)| w == W_A),
            "a miner already paid for this window is not paid again out of the debt"
        );

        // Recording the chunk that carries it clears the debt, and only by what
        // that chunk actually carries.
        deduct_owed(&mut owed, &plan);
        assert!(owed.is_empty());
    }

    #[test]
    fn an_owed_row_survives_a_restart_and_outlives_the_share_window() {
        // The debt is only as good as the file it lives in: a pool restarted
        // between the failed chunk and the next settlement would otherwise forget
        // it entirely, and the money would quietly rejoin the pot.
        let mut owed: Vec<(String, u64)> = Vec::new();
        owe_rows(&mut owed, &[(W_A.to_string(), 12), (W_B.to_string(), 3)]);
        // A second failure for the same miner adds to the debt, never replaces it.
        owe_rows(&mut owed, &[(W_A.to_string(), 8)]);
        assert_eq!(owed[0], (W_A.to_string(), 20));
        // A zero row is not a debt.
        owe_rows(&mut owed, &[("nobody".to_string(), 0)]);
        assert_eq!(owed.len(), 2);

        let back = parse_owed(&json!({ "owed": owed_to_json(&owed) }));
        assert_eq!(back, owed, "the ledger the pool writes is the one it reads");

        // A balance too small to clear the whole debt pays what it can and keeps
        // the rest, so one large debt cannot starve behind smaller ones.
        let (part, left) = take_owed(&owed, 5);
        assert_eq!(part, vec![(W_A.to_string(), 5)]);
        assert_eq!(left, 0, "nothing fresh is split while a debt is unpaid");
        let mut after = owed.clone();
        deduct_owed(&mut after, &part);
        assert_eq!(
            after,
            vec![(W_A.to_string(), 15), (W_B.to_string(), 3)],
            "what was not paid is still owed"
        );

        // With room to spare, the debt is cleared and the remainder is what the
        // fresh split runs on.
        let (all, left) = take_owed(&owed, 100);
        assert_eq!(all, owed);
        assert_eq!(left, 77);
    }

    #[test]
    fn a_miner_that_is_owed_and_also_has_shares_is_paid_once() {
        // Two actions to one address would spend two of the node's 200-action
        // budget for one miner, so a full chunk could be rejected outright for
        // being over the limit - which fails the whole chunk, which owes it all
        // over again.
        let mut owed: Vec<(String, u64)> = Vec::new();
        owe_rows(&mut owed, &[(W_A.to_string(), 10)]);
        let counts = vec![(W_A.to_string(), 1u64), (W_B.to_string(), 1u64)];
        let (mut plan, left) = take_owed(&owed, 110);
        assert_eq!(left, 100);
        plan.extend(plan_settlement(left, &counts));
        merge_payout_rows(&mut plan);
        assert_eq!(plan.len(), 2, "one action per miner");
        let paid: HashMap<String, u64> = plan.iter().cloned().collect();
        assert_eq!(paid[W_A], 60, "10 owed plus a 50 share, in one action");
        assert_eq!(paid[W_B], 50);
        assert_eq!(plan.iter().map(|(_, u)| *u).sum::<u64>(), 110);
        // And the debt clears by what it was owed, not by what the row carries.
        deduct_owed(&mut owed, &plan);
        assert!(owed.is_empty());
    }

    #[test]
    fn a_worker_is_told_what_a_failed_chunk_still_owes_it() {
        // A miner whose chunk failed and whose shares have since left the window
        // used to be answered "this pool has no record of that address" while the
        // pool was holding money for it.
        let mut p = a_pool();
        p.owed.push((W_A.to_string(), 30));
        p.matured = Some(Matured {
            units: 100,
            at: 1_500,
        });
        p.matured_current = true;
        let e = p.earnings_of(W_A, 1_000_000);
        assert!(e.known, "the pool owes it money, so it knows it");
        assert_eq!(e.shares, 0);
        assert_eq!(e.owed_units, 30);
        // The debt is not part of the pot the window is split over: it is already
        // assigned to a name. Counting it in both would promise it twice.
        assert_eq!(e.pool_pending_units, Some(70));
        let j = earnings_json(W_A, &e, 2_000);
        assert_eq!(j["owed"]["units"].as_u64(), Some(30));
        // The chain's own amount for 30 units of 0.1 HAC, normalized: 3 HAC.
        assert_eq!(j["owed"]["amount"].as_str(), Some("3:248"));
        // Someone else's debt is not this worker's money.
        let other = p.earnings_of(W_B, 1_000_000);
        assert_eq!(other.owed_units, 0);
        assert_eq!(other.pool_pending_units, Some(70));
    }

    #[test]
    fn paid_moves_only_when_the_node_buries_a_payout_and_only_upward() {
        let mut records: Vec<PayoutRecord> = Vec::new();
        let mut paid = PaidLedger::started(1_000);

        // Submitted is not paid, however sure the node sounds.
        records.push(a_payout("tx1", 1_100, true, &[(W_A, 10), (W_B, 5)]));
        assert_eq!(paid.total_units(), 0);
        assert_eq!(paid.workers(), 0);

        // Buried: credited, once, to the right people in the right amounts.
        confirm_payout(&mut records, &mut paid, "tx1", 1_200).expect("credited");
        assert_eq!(paid.get(W_A).unwrap().units, 10);
        assert_eq!(paid.get(W_B).unwrap().units, 5);
        assert_eq!(paid.total_units(), 15);
        assert_eq!(paid.get(W_A).unwrap().last_hash, "tx1");
        assert_eq!(paid.get(W_A).unwrap().last_at, 1_200);

        // A second payout adds; it never replaces.
        records.push(a_payout("tx2", 2_100, true, &[(W_A, 7)]));
        confirm_payout(&mut records, &mut paid, "tx2", 2_200).expect("credited");
        assert_eq!(paid.get(W_A).unwrap().units, 17, "totals only ever grow");
        assert_eq!(
            paid.get(W_B).unwrap().units,
            5,
            "untouched by someone else's payout"
        );
        // "last payout" is the most recent one, with its own amount and hash.
        assert_eq!(paid.get(W_A).unwrap().last_units, 7);
        assert_eq!(paid.get(W_A).unwrap().last_hash, "tx2");
        assert_eq!(paid.get(W_A).unwrap().last_at, 2_200);

        // A restart must not lose or double any of it.
        let restored = PaidLedger::from_json(&paid.to_json());
        assert_eq!(restored, paid);
        assert_eq!(restored.get(W_A).unwrap().units, 17);
        assert_eq!(restored.since, 1_000);
    }

    #[test]
    fn a_payout_row_survives_the_state_file_round_trip() {
        // The rows are what let a confirmed payout be credited to the right
        // miners. Losing them across a restart means a payout that confirms and
        // reaches nobody's total.
        let rec = a_payout("abc123", 4_242, true, &[(W_A, 12), (W_B, 3)]);
        let back = PayoutRecord::from_json(&rec.to_json()).expect("parsed");
        assert_eq!(back, rec);
        assert_eq!(back.units(), 15);
        assert_eq!(back.units_for(W_A), 12);
        assert_eq!(back.units_for("someone-else"), 0);
        // A row the file cannot describe is dropped, never guessed at.
        let broken = serde_json::json!({"hash":"h","at":1,"rows":[["a"],["b",4]]});
        let back = PayoutRecord::from_json(&broken).expect("parsed");
        assert_eq!(back.rows, vec![("b".to_string(), 4u64)]);
        assert!(!back.node_holds, "an absent verdict is not a confirmed one");
        assert!(PayoutRecord::from_json(&serde_json::json!({"at":1})).is_none());
    }

    /* ---- the stall alarm: loud only when a payout is really stuck ---- */

    #[test]
    fn a_healthy_payout_never_raises_the_stall_alarm() {
        // What this costs when it goes wrong: the alarm says nothing is being
        // paid. On mainnet defaults - 300s blocks and a 300s settle interval -
        // burial takes PAYOUT_MATURITY_DEPTH cycles, so a counter of settlement
        // cycles fires on cycles 3-6 of EVERY payout the pool ever makes, on a
        // pool that is working perfectly. The operator learns the warning is
        // noise, and the day a payout really is frozen - with every later payout
        // frozen behind it - the line saying so is the line they skip.
        //
        // The whole life of one healthy mainnet payout: one block to include it,
        // PAYOUT_MATURITY_DEPTH - 1 more to bury it, one block per settle cycle.
        for cycle in 1..=(PAYOUT_MATURITY_DEPTH + 1) {
            let blocks = cycle;
            let secs = cycle * 300;
            assert!(
                !payout_stalled(blocks, secs, 300),
                "cycle {cycle}: a payout {blocks} block(s) old is still burying, not stuck"
            );
        }
        // Same chain, a settle interval six times faster: the wait is a property
        // of the chain, so nothing about the verdict may change. This is why the
        // check cannot take `settle_secs` at all.
        for cycle in 1..=((PAYOUT_MATURITY_DEPTH + 1) * 6) {
            let blocks = cycle / 6;
            let secs = cycle * 50;
            assert!(
                !payout_stalled(blocks, secs, 300),
                "cycle {cycle}: a faster settle timer does not make a payout late"
            );
        }
        // A 10s testnet: 7 blocks in 70s is healthy there too.
        for cycle in 1..=(PAYOUT_MATURITY_DEPTH + 1) {
            assert!(!payout_stalled(cycle, cycle * 10, 10));
        }

        // ...and it does fire when the payout really is going nowhere.
        assert!(
            payout_stalled(STALLED_PAYOUT_BLOCKS, STALLED_PAYOUT_BLOCKS * 300, 300),
            "twice the burial depth in blocks, and still nothing: that is a stall"
        );
        // The tip has stopped moving, which no block count can ever see: zero
        // blocks elapse forever while every payout stays frozen.
        let budget = STALLED_PAYOUT_BLOCKS * 300 * STALLED_PAYOUT_TIME_SLACK;
        assert!(
            !payout_stalled(0, budget - 1, 300),
            "still inside the wall-clock budget"
        );
        assert!(
            payout_stalled(0, budget, 300),
            "a tip that has not moved in {budget}s is a dead chain, not a slow one"
        );
        // A nonsense chain parameter must not turn the time arm into a hair
        // trigger on a pool that is fine.
        assert!(!payout_stalled(1, 5, 0));
    }

    #[test]
    fn the_stall_warning_states_only_what_this_pool_measured() {
        // What this costs when it goes wrong: the warning is the only thing that
        // sends an operator to look at their pool. The text it replaced asserted
        // a cause it had never tested - that the pool's blocks carry only their
        // coinbase, so only another miner could ever confirm a payout - which
        // stopped being true the day the pool started packing the node's
        // transactions. Measured on the rig: the pool's own block carried 4
        // transactions, and a later one carried the payout itself. Sending the
        // operator off to fix peering on a node that is peered fine is worse
        // than saying nothing, because the real reason nobody is being paid then
        // goes unexamined.
        let mut wait = StallWait::default();
        wait.note(PayoutTxState::Pending);
        // Four packed transactions: exactly the pool block the rig produced.
        let msg = stall_warning(5, 1_500, &wait, 4);
        assert!(
            !msg.contains("nothing but its coinbase"),
            "the pool packs the node's transactions; never claim otherwise: {msg}"
        );
        assert!(
            !msg.contains("when another miner packs it"),
            "a block this pool mines can carry the payout: {msg}"
        );
        assert!(msg.contains("4 transaction(s) packed from the node"), "{msg}");
        // What IS observable: how long, and what it is waiting on.
        assert!(msg.contains("in 5 block(s) / 1500s"), "{msg}");
        assert!(
            msg.contains("1 in the node's mempool, waiting for a block to include it"),
            "{msg}"
        );

        // The same claim, on a pool where it is TRUE, is worth printing - and
        // only here, because only here was it checked.
        let coinbase_only = stall_warning(20, 6_000, &wait, 0);
        assert!(
            coinbase_only.contains("nothing but its coinbase"),
            "{coinbase_only}"
        );
        assert!(
            coinbase_only.contains("when another miner packs it"),
            "{coinbase_only}"
        );

        // A payout that is mined and burying says how much further it has to go,
        // and the shallowest one is the one that governs.
        let mut deep = StallWait::default();
        deep.note(PayoutTxState::Confirming(5));
        deep.note(PayoutTxState::Confirming(4));
        let msg = stall_warning(14, 4_200, &deep, 2);
        assert!(msg.contains("shallowest is 4 block(s) deep"), "{msg}");
        assert!(msg.contains("needs 2 more"), "{msg}");

        // Nothing is claimed about a payout the node would not describe.
        let mut quiet = StallWait::default();
        quiet.note(PayoutTxState::Unknown);
        quiet.note(PayoutTxState::Gone);
        let msg = stall_warning(13, 3_900, &quiet, 1);
        assert!(msg.contains("1 the node no longer holds"), "{msg}");
        assert!(
            msg.contains("1 whose state the node would not report"),
            "{msg}"
        );
        // Burial is the resolution, not a wait: it never reaches the summary.
        let mut done = StallWait::default();
        done.note(PayoutTxState::Buried(PAYOUT_MATURITY_DEPTH));
        assert_eq!(done, StallWait::default());
    }

    /* ---- what /earnings says, and what it refuses to say ---- */

    fn an_earnings() -> Earnings {
        Earnings {
            known: true,
            shares: 0,
            credit: 0,
            window_credit: 0,
            window_shares: 0,
            window_size: PPLNS_WINDOW as u64,
            paid: PaidRow::default(),
            paid_since: 1_000,
            inflight_units: 0,
            inflight: Vec::new(),
            owed_units: 0,
            pool_pending_units: Some(0),
            matured_at: 1_500,
            matured_current: true,
            unattributed_payouts: 0,
        }
    }

    #[test]
    fn an_unknown_worker_is_not_a_worker_that_is_owed_nothing() {
        // Showing "0 HAC owed" for an address the pool has never seen tells a
        // miner its work is being tracked when it is not.
        let unknown = Earnings {
            known: false,
            ..an_earnings()
        };
        let j = earnings_json(W_A, &unknown, 2_000);
        assert_eq!(j["kind"].as_str(), Some("unknown_worker"));
        assert_eq!(j["known"].as_bool(), Some(false));
        for absent in ["paid", "pending", "in_flight", "last_payout"] {
            assert!(j.get(absent).is_none(), "no money figures at all: {absent}");
        }

        // A worker the pool DOES know, owed nothing, says exactly that.
        let zero = an_earnings();
        let j = earnings_json(W_A, &zero, 2_000);
        assert_eq!(j["kind"].as_str(), Some("worker"));
        assert_eq!(j["known"].as_bool(), Some(true));
        assert_eq!(j["paid"]["units"].as_u64(), Some(0));
        assert_eq!(j["pending"]["known"].as_bool(), Some(true));
        assert_eq!(j["pending"]["units"].as_u64(), Some(0));
        assert_eq!(j["in_flight"]["units"].as_u64(), Some(0));
        assert!(j["last_payout"].is_null(), "never paid is null, not 0");
    }

    #[test]
    fn a_bad_address_is_a_third_answer_again() {
        assert_eq!(classify_worker_id(W_A), WorkerId::Payable);
        assert_eq!(classify_worker_id("not-an-address"), WorkerId::NotAnAddress);
        assert_eq!(classify_worker_id(""), WorkerId::NotAnAddress);
        // A well-formed address this pool could never pay is its own case: the
        // pool refuses to credit shares to it at all, so reporting it as an
        // unknown worker would hide WHY it will never earn anything.
        let mut raw = [0u8; 21];
        raw[0] = 1; // CONTRACT version
        raw[1..].copy_from_slice(&[9u8; 20]);
        let contract = Address::from(raw).to_readable();
        assert_eq!(classify_worker_id(&contract), WorkerId::Unpayable);
    }

    #[test]
    fn pending_is_unknown_not_zero_when_the_pool_cannot_value_its_wallet() {
        // "0 HAC owed" and "the pool cannot say" are different facts, and the
        // second must never be rendered as the first.
        let blind = Earnings {
            pool_pending_units: None,
            shares: 10,
            window_shares: 10,
            ..an_earnings()
        };
        let j = earnings_json(W_A, &blind, 2_000);
        assert_eq!(j["pending"]["known"].as_bool(), Some(false));
        assert!(j["pending"].get("units").is_none(), "no number to misread");
        assert!(j["pending"].get("amount").is_none());
        assert!(
            j["pending"]["reason"]
                .as_str()
                .unwrap()
                .contains("not a zero"),
            "{}",
            j["pending"]
        );
        // The rest of the answer still stands: paid and in flight are the pool's
        // own ledger and do not depend on reading the wallet.
        assert_eq!(j["paid"]["units"].as_u64(), Some(0));

        // A known zero says so, and says how old the figure is.
        let known_zero = Earnings {
            pool_pending_units: Some(0),
            shares: 10,
            window_shares: 10,
            ..an_earnings()
        };
        let j = earnings_json(W_A, &known_zero, 2_000);
        assert_eq!(j["pending"]["known"].as_bool(), Some(true));
        assert_eq!(j["pending"]["units"].as_u64(), Some(0));
        assert_eq!(j["pending"]["estimate"].as_bool(), Some(true));
        assert_eq!(j["pending"]["as_of_age_secs"].as_u64(), Some(500));
        assert_eq!(j["pending"]["current"].as_bool(), Some(true));
    }

    #[test]
    fn pending_is_unknown_while_a_tracked_payout_has_no_recipient_detail() {
        // A payout hash left by an older build, or by a tool that recorded only
        // the hash. Its units are still inside the node's confirmed balance, so
        // the in-flight subtraction is incomplete and pending would be too HIGH -
        // the one direction that promises a miner money.
        let e = Earnings {
            shares: 1,
            window_shares: 1,
            // The only share in the window, and the only credit in it.
            credit: 5_000,
            window_credit: 5_000,
            pool_pending_units: Some(100),
            unattributed_payouts: 1,
            ..an_earnings()
        };
        let j = earnings_json(W_A, &e, 2_000);
        assert_eq!(j["pending"]["known"].as_bool(), Some(false));
        assert!(j["pending"].get("units").is_none());
        let why = j["pending"]["reason"].as_str().expect("a reason");
        assert!(why.contains("not a zero"), "{why}");
        assert!(why.contains("overstate"), "{why}");
        // Paid and in flight are the pool's own ledger and still stand.
        assert_eq!(j["paid"]["units"].as_u64(), Some(0));
        // Once the rows are known, the same worker gets its figure.
        let ok = Earnings {
            unattributed_payouts: 0,
            ..e
        };
        let j = earnings_json(W_A, &ok, 2_000);
        assert_eq!(j["pending"]["units"].as_u64(), Some(100));
    }

    #[test]
    fn money_is_reported_as_the_chains_own_amount() {
        // Never a float, never a decimal this pool rounded itself: the same
        // mantissa:unit string the node speaks and the transaction carries.
        let e = Earnings {
            paid: PaidRow {
                units: 35,
                last_units: 35,
                last_hash: "tx1".to_string(),
                last_at: 1_900,
            },
            inflight_units: 4,
            inflight: vec![InflightRow {
                hash: "tx2".to_string(),
                units: 4,
                at: 1_950,
                node_holds: true,
            }],
            shares: 2,
            window_shares: 4,
            // Two shares of the four, each in the window for the same time, so
            // this worker is owed half of what the window is owed.
            credit: 2_000,
            window_credit: 4_000,
            pool_pending_units: Some(10),
            ..an_earnings()
        };
        let j = earnings_json(W_A, &e, 2_000);
        assert_eq!(j["paid"]["amount"].as_str(), Some("35:247"));
        assert_eq!(j["paid"]["units"].as_u64(), Some(35));
        assert_eq!(j["last_payout"]["amount"].as_str(), Some("35:247"));
        assert_eq!(j["last_payout"]["tx"].as_str(), Some("tx1"));
        assert_eq!(j["last_payout"]["at_unix"].as_u64(), Some(1_900));
        assert_eq!(j["in_flight"]["amount"].as_str(), Some("4:247"));
        assert_eq!(j["in_flight"]["txs"][0]["node_holds"].as_bool(), Some(true));
        // 10 units over a window worth 4_000ms of credit, holding 2_000 of it: 5.
        assert_eq!(j["pending"]["units"].as_u64(), Some(5));
        assert_eq!(j["pending"]["amount"].as_str(), Some("5:247"));
        // The chain normalizes 100 units of 0.1 HAC to 10 HAC, and so does this.
        assert_eq!(payout_amount(100).to_fin_string(), "1:249");
        assert_eq!(payout_amount(0).to_fin_string(), "0:0");
    }

    #[test]
    fn a_pending_estimate_never_promises_more_than_a_settlement_would_pay() {
        // The estimate is the floor share. `split_payout` may hand one extra unit
        // to the largest remainders, so the estimate is at or below what a
        // settlement at this instant pays - never above it.
        let counts = vec![(W_A.to_string(), 3u64), (W_B.to_string(), 1u64)];
        let split: HashMap<String, u64> = plan_settlement(100, &counts).into_iter().collect();
        assert_eq!(worker_pending_units(100, 3, 4), split[W_A]);
        assert_eq!(worker_pending_units(100, 1, 4), split[W_B]);

        let thirds = vec![
            (W_A.to_string(), 1u64),
            (W_B.to_string(), 1u64),
            ("c".to_string(), 1u64),
        ];
        for (_, paid) in plan_settlement(100, &thirds) {
            assert!(
                worker_pending_units(100, 1, 3) <= paid,
                "the estimate must never be the rounded-UP share"
            );
        }

        // Nothing to split, nothing held, or an empty window: zero, honestly.
        assert_eq!(worker_pending_units(0, 5, 10), 0);
        assert_eq!(worker_pending_units(100, 0, 10), 0);
        assert_eq!(worker_pending_units(100, 5, 0), 0);
        // A share below the advertised minimum is paid nothing at all, so the
        // estimate must not promise a fraction of it.
        assert_eq!(worker_pending_units(1, 1, 1_000), 0);
        // And it does not overflow at the top of the range.
        assert_eq!(worker_pending_units(u64::MAX, 1, 1), u64::MAX);
    }

    /// The pool's own wallet in tests, from a fixed key so the address is stable.
    ///
    /// Held the way startup holds it: ONE account, and `payout` is its address.
    /// A fixture where the two disagree would quietly bless the exact state this
    /// pool must never be in - mining to one address and paying out of another.
    fn a_wallet() -> Arc<Account> {
        Arc::new(Account::create_by_secret_key_value([0x11u8; 32]).expect("a valid test key"))
    }

    /// A pool with no node, no disk and no listener: enough to exercise the
    /// accounting the endpoints read.
    fn a_pool() -> Pool {
        let tpl = a_template(PackedTxs::default());
        let acc = a_wallet();
        Pool {
            node: String::new(),
            payout: acc.readable().to_string(),
            acc,
            state_file: String::new(), // no disk: state_shot returns None
            client: http_client(),
            params: ChainParams::mainnet(),
            share_target: [0xff; 32],
            network_target: tpl.target,
            tpl,
            share_factor: 24,
            share_factor_achieved: 24,
            share_cost_bits: 16,
            share_halt: None,
            pending_cache: String::new(),
            workers: HashMap::new(),
            next_en: 0,
            pplns: Pplns::new(PPLNS_WINDOW, pplns_horizon_ms(300)),
            accepted: 0,
            blocks: 0,
            orphaned: 0,
            seen: HashSet::new(),
            submitted: Vec::new(),
            immature: Vec::new(),
            unsaved: 0,
            state_seq: 0,
            settle_pending_txs: Vec::new(),
            payout_records: Vec::new(),
            inflight_units: 0,
            owed: Vec::new(),
            paid: PaidLedger::started(1_000),
            matured: None,
            matured_current: false,
            settle_secs: 300,
            stall_since: None,
            bad_streak: HashMap::new(),
            tpl_changed_at_ms: 0,
            rates: HashMap::new(),
            share_log: ShareLog::default(),
        }
    }

    #[test]
    fn a_workers_pending_never_repeats_money_that_is_already_in_flight() {
        // The node's CONFIRMED balance still contains a payout that is sitting in
        // the mempool. Reporting the matured balance as pending while the same
        // units are reported as in flight would show a miner its money twice.
        let mut p = a_pool();
        // Four shares that all landed at the same instant, read a second later:
        // equal ages, so credit is in the same 3:1 ratio as the headcount.
        let t0 = 1_000_000u64;
        for _ in 0..3 {
            p.pplns.record(W_A, t0);
        }
        p.pplns.record(W_B, t0);
        let now = t0 + 1_000;
        // 100 units matured, of which 40 are already inside a submitted payout.
        p.matured = Some(Matured {
            units: 100,
            at: 1_500,
        });
        p.matured_current = true;
        p.payout_records
            .push(a_payout("tx1", 1_400, true, &[(W_A, 30), (W_B, 10)]));
        p.rebuild_inflight();
        assert_eq!(p.inflight_units, 40);

        let a = p.earnings_of(W_A, now);
        let b = p.earnings_of(W_B, now);
        // Pool-wide pending is 100 - 40 = 60, split 3:1 by credit.
        assert_eq!(a.pool_pending_units, Some(60));
        assert_eq!(worker_pending_units(60, a.credit, a.window_credit), 45);
        assert_eq!(worker_pending_units(60, b.credit, b.window_credit), 15);
        assert_eq!((a.shares, a.inflight_units), (3, 30));
        assert_eq!((b.shares, b.inflight_units), (1, 10));
        // Everything the pool holds is accounted for exactly once: 45 + 15
        // pending, 30 + 10 in flight, 0 paid, and that is the whole 100.
        assert_eq!(45 + 15 + 30 + 10, 100);

        // Confirming the payout moves 40 from in flight to paid and takes it out
        // of the balance; pending is unchanged, because it never contained it.
        let mut guard = p;
        let pp = &mut guard;
        confirm_payout(&mut pp.payout_records, &mut pp.paid, "tx1", 1_600).expect("credited");
        pp.rebuild_inflight();
        pp.matured = Some(Matured {
            units: 60,
            at: 1_650,
        });
        let a = pp.earnings_of(W_A, now);
        assert_eq!(a.inflight_units, 0);
        assert_eq!(a.paid.units, 30);
        assert_eq!(a.pool_pending_units, Some(60));
        assert_eq!(worker_pending_units(60, a.credit, a.window_credit), 45);
    }

    #[test]
    fn the_pool_can_tell_a_worker_it_has_never_seen_from_one_it_owes_nothing() {
        let mut p = a_pool();
        // Never heard of: no shares, no payments, no work handed out.
        assert!(!p.earnings_of(W_A, 1_000_000).known);
        // A worker that fetched work but has not found a share yet IS known: the
        // pool is tracking it, it simply has nothing yet.
        p.extranonce_for(W_A);
        let e = p.earnings_of(W_A, 1_000_000);
        assert!(e.known);
        assert_eq!((e.shares, e.paid.units, e.inflight_units), (0, 0, 0));
        // A worker whose shares have all been evicted from the window is still
        // known while it has a paid history: its money did not stop existing.
        let mut q = a_pool();
        q.payout_records
            .push(a_payout("tx1", 1_100, true, &[(W_B, 12)]));
        q.rebuild_inflight();
        confirm_payout(&mut q.payout_records, &mut q.paid, "tx1", 1_200);
        let e = q.earnings_of(W_B, 1_000_000);
        assert!(e.known);
        assert_eq!(e.shares, 0);
        assert_eq!(e.paid.units, 12);
        assert_eq!(e.paid.last_hash, "tx1");
    }

    /* ---- the terms the pool advertises are the terms it applies ---- */

    #[test]
    fn the_advertised_terms_are_the_terms_the_settlement_applies() {
        let t = terms_json(
            PPLNS_WINDOW as u64,
            pplns_horizon_ms(300),
            24,
            24,
            16,
            None,
            0x2000_0000,
            300,
            COINBASE_MATURITY_DEPTH,
            PAYOUT_MATURITY_DEPTH,
        );
        // It is PPLNS, and it says so without leaving room to read PROP into it.
        assert_eq!(t["scheme"].as_str(), Some("PPLNS"));
        assert!(
            t["scheme_note"].as_str().unwrap().contains("NOT PROP"),
            "{t}"
        );
        assert_eq!(t["window_shares"].as_u64(), Some(PPLNS_WINDOW as u64));

        // The advertised fee is the fee the split applies.
        let fee = t["fee"]["units"].as_u64().expect("fee");
        let counts = vec![(W_A.to_string(), 3u64), (W_B.to_string(), 1u64)];
        let split = plan_settlement(100, &counts);
        assert_eq!(
            split.iter().map(|(_, u)| *u).sum::<u64>(),
            100 - fee,
            "everything but the advertised fee reaches the miners"
        );

        // The advertised minimum is the minimum the split enforces.
        let min = t["minimum_payout"]["units"].as_u64().expect("minimum");
        let lopsided = vec![(W_A.to_string(), 10_000u64), (W_B.to_string(), 1u64)];
        for (_, u) in plan_settlement(1_000, &lopsided) {
            assert!(
                u >= min,
                "nothing below the advertised minimum is ever paid"
            );
        }

        // Every money figure is the chain's own amount, matching its own units.
        for key in ["fee", "minimum_payout", "fee_reserve"] {
            let units = t[key]["units"].as_u64().expect(key);
            assert_eq!(
                t[key]["amount"].as_str(),
                Some(payout_amount(units).to_fin_string().as_str()),
                "{key}"
            );
        }
        assert_eq!(
            t["network_fee_per_settlement_tx"].as_str(),
            Some(chunk_tx_fee().to_fin_string().as_str())
        );
        // The maturity depths are the ones the code waits for, not a description.
        assert_eq!(
            t["coinbase_maturity_blocks"].as_u64(),
            Some(COINBASE_MATURITY_DEPTH)
        );
        assert_eq!(
            t["payout_confirm_blocks"].as_u64(),
            Some(PAYOUT_MATURITY_DEPTH)
        );
        assert_eq!(t["settle_interval_secs"].as_u64(), Some(300));
        assert_eq!(
            t["recipients_per_settlement_tx"].as_u64(),
            Some(PAYOUT_CHUNK as u64)
        );
    }

    /* ---- the share target has to be worth something ---- */

    #[test]
    fn a_share_target_the_live_difficulty_cannot_support_is_refused() {
        // Observed on a live run: one worker took the ENTIRE 4096-share window.
        // `check_share_factor` passed, because the factor the operator typed was
        // fine - but at the difficulty in force the derived share target had
        // saturated, every hash was a valid share, and the number of shares a
        // worker was credited with measured how fast it could submit.
        assert!(check_share_factor(24).is_ok());

        let net = pool_core::network_target_hash(LOWEST_DIFFICULTY);
        let served = pool_core::share_target_hash(LOWEST_DIFFICULTY, 24);
        let achieved = pool_core::achieved_share_factor(&net, &served);
        assert_eq!(achieved, 0, "24 asked for, 0 actually served");
        let cost = pool_core::share_cost_bits(&served);
        let e = check_share_target(24, achieved, cost, LOWEST_DIFFICULTY).expect_err("refused");
        assert!(e.contains("too low"), "{e}");
        assert!(e.contains("submit"), "{e}");

        // A difficulty with room to shift keeps the factor the operator chose.
        // (The top byte encodes 255 - leading_zero_bits, so this target has 223
        // leading zeros and 24 significant bits: 24 powers of two to spare.)
        let d = 0x20FF_FFFFu32;
        let net = pool_core::network_target_hash(d);
        let served = pool_core::share_target_hash(d, 24);
        assert_eq!(pool_core::achieved_share_factor(&net, &served), 24);
        assert!(check_share_target(24, 24, pool_core::share_cost_bits(&served), d).is_ok());
    }

    #[test]
    fn a_healthy_ratio_over_a_free_share_is_refused() {
        // The hole the ratio bound alone left open, and it is not hypothetical:
        // it is the ASERT activation target every non-mainnet chain lands on.
        // 0xe9cfffff has 255 - 0xe9 = 22 leading zero bits, so asking for 24
        // saturates to the all-0xff ceiling. The achieved factor then reads 22,
        // sails past MIN_SHARE_FACTOR, and EVERY hash beats the share target.
        // The pool would have started and credited HTTP round trips as work.
        let d = 0xe9cf_ffffu32;
        let net = pool_core::network_target_hash(d);
        let served = pool_core::share_target_hash(d, 24);
        assert_eq!(served, [0xff; 32], "saturated to the ceiling");
        let achieved = pool_core::achieved_share_factor(&net, &served);
        let cost = pool_core::share_cost_bits(&served);
        assert_eq!(achieved, 22, "the ratio looks fine");
        assert!(achieved >= MIN_SHARE_FACTOR, "and clears the ratio bound");
        assert_eq!(cost, 0, "while a share costs one hash");

        let e = check_share_target(24, achieved, cost, d).expect_err("refused");
        assert!(e.contains("worth counting"), "{e}");
        // 22 bits of chain, 18 the lowest legal factor: 4 is the most a share
        // could ever cost here, so lowering share_bits cannot rescue it.
        assert!(e.contains("nothing here helps"), "{e}");

        // Where the chain DOES have room, the advice is the opposite, and names
        // the number: 40 bits of work, 16 needed for the share, so 24 or less.
        let d = 0xd7ff_ffffu32; // 255 - 0xd7 = 40 leading zero bits
        let net = pool_core::network_target_hash(d);
        let served = pool_core::share_target_hash(d, 30);
        let achieved = pool_core::achieved_share_factor(&net, &served);
        let cost = pool_core::share_cost_bits(&served);
        assert_eq!((achieved, cost), (30, 10));
        let e = check_share_target(30, achieved, cost, d).expect_err("refused");
        assert!(e.contains("lower <share_bits> to 24 or less"), "{e}");

        // Spending less on frequency leaves enough on the share itself.
        let served = pool_core::share_target_hash(d, 24);
        let achieved = pool_core::achieved_share_factor(&net, &served);
        let cost = pool_core::share_cost_bits(&served);
        assert_eq!((achieved, cost), (24, 16));
        assert!(check_share_target(24, achieved, cost, d).is_ok());
    }

    #[test]
    fn a_difficulty_fall_under_a_running_pool_stops_credit_and_stops_paying() {
        // Startup applied BOTH bounds and refused to start. The running path did
        // not: it re-derived the ratio only, compared the ratio alone, fired only
        // on the edge, and put its verdict in an eprintln that nothing read. So a
        // pool that started healthy and watched the difficulty fall went on
        // crediting free shares and went on settling on them - paying whoever
        // submitted fastest with money the miners who hashed had earned. A payout
        // is a signed transfer; nobody gets it back.

        // 40 leading zero bits: 24 of them buy the ratio, 16 are left for what a
        // share itself costs. Healthy at share_bits 24.
        let healthy = 0xd7ff_ffffu32;
        // 22 leading zero bits: asking for 24 saturates the share target to
        // all-0xff, so EVERY hash is a share - while the ratio reads 22 and sails
        // past MIN_SHARE_FACTOR. This is the case the ratio-only runtime check
        // could not see, and it is the ASERT activation target, not a contrivance.
        let free = 0xe9cf_ffffu32;

        let mut p = a_pool();
        p.share_factor = 24;
        p.tpl.difficulty = healthy;
        p.tpl.target = pool_core::network_target_hash(healthy);
        p.network_target = p.tpl.target;
        p.recompute_share_target();
        assert_eq!((p.share_factor_achieved, p.share_cost_bits), (24, 16));
        assert!(p.share_halt.is_none(), "a healthy chain credits normally");

        // The chain's difficulty falls while miners are connected.
        p.tpl.difficulty = free;
        p.tpl.target = pool_core::network_target_hash(free);
        p.network_target = p.tpl.target;
        p.recompute_share_target();
        assert_eq!(p.share_target, [0xff; 32], "every hash is a share now");
        assert!(
            p.share_factor_achieved >= MIN_SHARE_FACTOR,
            "and the ratio the old runtime check looked at still reads healthy"
        );
        assert_eq!(p.share_cost_bits, 0, "while a share costs one hash");
        let why = p.share_halt.clone().expect("the pool has to stop");
        assert!(why.contains("worth counting"), "{why}");

        // The cost figure is published beside the ratio, and the terms no longer
        // claim a refusal that never happened: they state what is really true now.
        let t = terms_json(
            p.pplns.window() as u64,
            p.pplns.horizon_ms(),
            p.share_factor,
            p.share_factor_achieved,
            p.share_cost_bits,
            p.share_halt.as_deref(),
            p.tpl.difficulty,
            p.settle_secs,
            COINBASE_MATURITY_DEPTH,
            PAYOUT_MATURITY_DEPTH,
        );
        assert_eq!(t["share_cost_bits"].as_u64(), Some(0));
        assert_eq!(t["crediting_shares"].as_bool(), Some(false));
        assert_eq!(t["crediting_halt_reason"].as_str(), Some(why.as_str()));

        // BINDING, not advisory. A submission every piece of hardware on earth
        // produces on the first try is refused with the reason, and nothing
        // enters the window it would have been paid out of.
        let height = p.tpl.height;
        p.matured = Some(Matured {
            units: 1_000,
            at: 1_500,
        });
        p.matured_current = true;
        // A node that is up and answering, so the halt is demonstrably what
        // stops the settlement rather than the absence of a node.
        let (node, seen) = a_stub_node();
        p.node = node;
        // ...and a window worth splitting, so there is money on the table.
        p.pplns
            .record(W_B, pool_core::now_ms().saturating_sub(60_000));
        let pool = Arc::new(Mutex::new(p));
        let r = handle_submission(&pool, W_A, height, [0x11u8; 32], 7);
        assert_eq!(r["ok"].as_bool(), Some(false), "{r}");
        assert_eq!(r["kind"].as_str(), Some("degraded"), "{r}");
        assert_eq!(
            r["err"].as_str(),
            Some(why.as_str()),
            "a miner has to be told why its shares stopped counting: {r}"
        );
        {
            let g = plock(&pool);
            assert_eq!(g.accepted, 0, "a free share may not be credited");
            assert_eq!(g.pplns.count_of(W_A), 0, "and may not enter the window");
        }

        // And the settlement that would have split real money by that window
        // pays nothing. It stops before it values the wallet, so it never asks
        // the node for a balance and never signs a transfer - which is also what
        // leaves `matured_current` alone, since only the paths that try to value
        // the wallet and fail clear it.
        settle_once(&pool);
        let asked = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            !asked.iter().any(|p| p.starts_with("/query/balance")),
            "a halted pool must not even value its wallet: {asked:?}"
        );
        assert!(
            !asked
                .iter()
                .any(|p| p.starts_with("/submit/transaction")),
            "a halted pool must not submit anything: {asked:?}"
        );
        {
            let g = plock(&pool);
            assert!(g.settle_pending_txs.is_empty(), "nothing may be signed");
            assert!(
                g.matured_current,
                "settle_once has to stop before it values anything, let alone pays"
            );
        }

        // Not a one-way door: the same derivation clears the halt when the
        // difficulty recovers, so an operator is not left restarting a pool that
        // is already fine again.
        {
            let mut g = plock(&pool);
            g.tpl.difficulty = healthy;
            g.tpl.target = pool_core::network_target_hash(healthy);
            g.network_target = g.tpl.target;
            g.recompute_share_target();
            assert!(g.share_halt.is_none(), "crediting resumes by itself");
            assert_eq!((g.share_factor_achieved, g.share_cost_bits), (24, 16));
        }
    }

    #[test]
    fn a_halted_pool_still_finishes_resolving_the_payouts_it_already_made() {
        // Damage one fix did to another. The payout resolution runs before the
        // "nothing to settle" exit on purpose - it is what turns a miner's IN
        // FLIGHT into PAID when the node reports the paying transaction buried,
        // and skipping it leaves a miner's last payout reading "in flight" for
        // ever. The difficulty halt was then added at the TOP of the cycle, in
        // front of it, so a pool that halted with a payout outstanding froze that
        // payout for the whole length of the halt: never credited as paid, and a
        // failed chunk's rows never reaching the owed ledger either.
        //
        // Resolving is not paying. The halt belongs after it, before anything is
        // valued or signed.
        let (node, seen) = a_stub_node_answering(vec![(
            "/query/transaction",
            r#"{"ret":0,"confirm":6}"#, // buried at exactly the maturity depth
        )]);
        let mut p = a_pool();
        p.node = node;
        p.share_halt = Some("a share costs no work on this chain".to_string());
        p.payout_records
            .push(a_payout("aa11", 1_000, true, &[(W_A, 25)]));
        p.settle_pending_txs.push("aa11".to_string());
        p.rebuild_inflight();
        // Something a split would have paid, so "nothing was paid" cannot be an
        // accident of an empty window.
        p.pplns
            .record(W_B, pool_core::now_ms().saturating_sub(60_000));
        p.matured = Some(Matured {
            units: 1_000,
            at: 1_500,
        });
        let pool = Arc::new(Mutex::new(p));

        settle_once(&pool);

        let g = plock(&pool);
        assert_eq!(
            g.paid.get(W_A).map(|r| r.units),
            Some(25),
            "a buried payout has to be credited even while crediting is halted"
        );
        assert!(g.settle_pending_txs.is_empty(), "and stop being tracked");
        assert_eq!(g.inflight_units, 0);
        // ...and still nothing fresh was valued or signed.
        let asked = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            !asked.iter().any(|p| p.starts_with("/query/balance")),
            "a halted pool must not value its wallet: {asked:?}"
        );
        assert!(
            !asked
                .iter()
                .any(|p| p.starts_with("/submit/transaction")),
            "a halted pool must not submit anything: {asked:?}"
        );
    }

    /// A stand-in node that answers just enough for one settlement cycle and
    /// records every path it was asked for. Returns its base URL and that log.
    ///
    /// It answers the tip, so the cycle gets past its proof-of-life, and refuses
    /// everything else, so the cycle stops at the first thing it cannot value -
    /// before it signs or submits anything. What the test reads is which ADDRESS
    /// the cycle asked about on the way there.
    fn a_stub_node() -> (String, Arc<Mutex<Vec<String>>>) {
        a_stub_node_answering(Vec::new())
    }

    /// The same stub, with extra (path prefix -> body) answers tried before the
    /// blanket refusal, so a test can put the node in one specific state.
    fn a_stub_node_answering(
        extra: Vec<(&'static str, &'static str)>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a stub node");
        let base = format!("http://{}", listener.local_addr().expect("stub address"));
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for s in listener.incoming() {
                let Ok(mut s) = s else { continue };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                log.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(path.clone());
                let body = if path.starts_with("/query/latest") {
                    r#"{"height":100}"#
                } else {
                    extra
                        .iter()
                        .find(|(p, _)| path.starts_with(p))
                        .map(|(_, b)| *b)
                        .unwrap_or(r#"{"ret":1,"errmsg":"stub refuses everything else"}"#)
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
                let _ = s.shutdown(Shutdown::Both);
            }
        });
        (base, seen)
    }

    #[test]
    fn seeing_a_payout_in_the_mempool_is_recorded_so_a_later_loss_is_not_read_as_never_sent() {
        // The gap this closes: `node_holds` was written in exactly one place -
        // straight after a submit whose verification came back `Held`. A payout
        // submitted through a TIMEOUT skips that line entirely, and a
        // verification that could not be read skips it too. Both leave the pool
        // holding a signed transaction the node may well have taken and relayed,
        // marked "never held".
        //
        // The next cycle then sees it sitting in the mempool - proof it is on
        // the network - and used to write nothing down. So the day the node was
        // restarted and answered "I do not know that hash", the pool read the
        // whole thing as never relayed, put the rows back on the owed ledger and
        // signed a SECOND transaction for the same window. Different timestamp,
        // different hash, replay protection by hash alone: both mineable, the
        // operator pays twice.
        let (node, _seen) = a_stub_node_answering(vec![(
            "/query/transaction",
            r#"{"ret":0,"pending":true}"#, // the node is holding it right now
        )]);
        let mut p = a_pool();
        p.node = node;
        let rec = a_payout("aa11", 1_000, false, &[(W_A, 25)]);
        assert!(!rec.node_holds, "this is the record a timed-out submit leaves");
        p.payout_records.push(rec);
        p.settle_pending_txs.push("aa11".to_string());
        p.rebuild_inflight();
        let pool = Arc::new(Mutex::new(p));

        settle_once(&pool);

        let g = plock(&pool);
        assert!(
            g.payout_records[0].node_holds,
            "the pool saw the node holding this payout and did not write it down"
        );
        assert_eq!(
            g.settle_pending_txs,
            vec!["aa11".to_string()],
            "a payout in the mempool stays tracked"
        );
        assert!(
            g.owed.is_empty(),
            "nothing is owed while the payout is still in flight"
        );
        assert_eq!(g.inflight_units, 25);
    }

    #[test]
    fn a_settlement_values_and_signs_from_the_one_wallet_the_pool_mines_to() {
        // What this costs when it is wrong: the settlement thread used to re-read
        // the key file from disk on every cycle. Two ways that took money.
        //
        // If the file was momentarily ABSENT - renamed by a backup, quarantined
        // by antivirus, on a share that blinked - the loader made a fresh random
        // wallet and the cycle valued and signed from THAT. The pool went on
        // mining to the address pinned at startup, so the node truthfully
        // reported nothing for the new one, and every miner was told in the
        // pool's own voice, marked current, that it was owed nothing. Real income
        // kept landing in an address the settlement had stopped looking at.
        //
        // And any OTHER read failure - a sharing violation is the everyday one on
        // Windows - called process::exit from inside this thread. That is not an
        // unwind, so the catch_unwind the settlement loop is wrapped in cannot
        // see it: the pool just stops, possibly with a payout in the mempool.
        //
        // The wallet is now read once at startup and held on `Pool`, so the only
        // address a cycle can ask about is the one it mines to, and no cycle
        // touches the key file at all.
        let (node, seen) = a_stub_node();
        let mut p = a_pool();
        p.node = node;
        // Give it something to settle, or it stops before it values anything.
        p.pplns
            .record(W_B, pool_core::now_ms().saturating_sub(60_000));
        let mine = p.payout.clone();
        let pool = Arc::new(Mutex::new(p));

        settle_once(&pool);

        let asked = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let valued: Vec<&String> = asked
            .iter()
            .filter(|p| p.starts_with("/query/balance"))
            .collect();
        assert_eq!(
            valued.len(),
            1,
            "one cycle values the wallet once: {asked:?}"
        );
        assert!(
            valued[0].contains(&mine),
            "the settlement valued {} while the pool mines to {mine}",
            valued[0]
        );
        // And it stopped there. An answer the pool cannot value is never a zero
        // balance, so nothing was signed and miners are told the figure is stale.
        let g = plock(&pool);
        assert!(
            !g.matured_current,
            "an unvaluable balance has to be reported STALE, never as zero"
        );
        assert!(
            g.settle_pending_txs.is_empty(),
            "nothing may be signed off a balance the pool could not read"
        );
    }

    #[test]
    fn a_same_height_reorg_keeps_already_credited_solutions() {
        // Clearing `seen` on a same-height template swap let an A -> B -> A flap
        // re-admit solutions that were already credited, doubling their PPLNS
        // weight at every honest miner's expense.
        let mut seen: HashSet<(u64, [u8; 32], u32)> = HashSet::new();
        seen.insert((100, [7u8; 32], 42));
        seen.insert((100, [8u8; 32], 43));
        seen.insert((99, [7u8; 32], 1));
        prune_seen(&mut seen, 100); // same height, different prev-hash
        assert!(seen.contains(&(100, [7u8; 32], 42)));
        assert!(seen.contains(&(100, [8u8; 32], 43)));
        assert!(
            !seen.contains(&(99, [7u8; 32], 1)),
            "stale heights are pruned"
        );
        // A height advance drops everything: those keys fail the freshness check
        // anyway, so keeping them would only grow memory.
        prune_seen(&mut seen, 101);
        assert!(seen.is_empty());
    }

    #[test]
    fn a_block_is_only_confirmed_once_the_chain_has_buried_it() {
        // Counting a block the moment it merely occupies the tip also stopped us
        // watching it, so a shallow reorg of one of OUR blocks could never be
        // detected and blocks_confirmed over-counted for good.
        let h = 1_000u64;
        assert!(
            !buried_deep(Some(h), h),
            "0 blocks stacked on top is not buried"
        );
        assert!(!buried_deep(Some(h + COINBASE_MATURITY_DEPTH - 1), h));
        assert!(buried_deep(Some(h + COINBASE_MATURITY_DEPTH), h));
        // No tip this cycle: keep watching rather than finalize on a guess.
        assert!(!buried_deep(None, h));
    }

    #[test]
    fn share_size_that_would_unbalance_the_payout_window_is_refused() {
        // Shares are credited with equal weight, which is only fair while the
        // whole window is much shorter than one block interval.
        assert!(check_share_factor(24).is_ok()); // documented default
        assert!(check_share_factor(MIN_SHARE_FACTOR).is_ok());
        assert!(check_share_factor(MAX_SHARE_FACTOR).is_ok());
        assert!(check_share_factor(MIN_SHARE_FACTOR - 1).is_err());
        assert!(check_share_factor(0).is_err());
        assert!(check_share_factor(MAX_SHARE_FACTOR + 1).is_err());
    }

    #[test]
    fn a_chunk_the_node_never_took_is_not_reported_as_paid() {
        // Observed: the pool printed "settlement done: 2 recipient(s), 35 units
        // across 1 tx(s)" for a payout the node never held. Nobody was paid and
        // the pool wallet never moved, but the summary said money had gone out.
        // Only what the node actually holds may be counted.
        let mut t = SettleTally::default();
        t.record(ChunkOutcome::Failed, 2, 35);
        assert_eq!(t.recipients, 0, "a rejected chunk pays nobody");
        assert_eq!(t.units, 0, "a rejected chunk moves no money");
        assert_eq!(
            t.txs, 0,
            "the tx count is what the node took, not what we tried"
        );
        assert_eq!((t.failed_txs, t.failed_units), (1, 35));
        assert!(!t.all_delivered());
        let s = t.summary();
        assert!(
            s.contains("0 recipient(s), 0 unit(s) across 0 tx(s)"),
            "{s}"
        );
        assert!(s.contains("FAILED"), "{s}");
        assert!(s.contains("still owed"), "{s}");
    }

    #[test]
    fn a_mixed_settlement_separates_delivered_from_owed() {
        let mut t = SettleTally::default();
        t.record(ChunkOutcome::Delivered, 190, 400);
        t.record(ChunkOutcome::Failed, 12, 30);
        t.record(ChunkOutcome::Unresolved, 5, 11);
        assert_eq!((t.recipients, t.units, t.txs), (190, 400, 1));
        assert_eq!((t.failed_txs, t.failed_units), (1, 30));
        assert_eq!((t.unresolved_txs, t.unresolved_units), (1, 11));
        assert!(!t.all_delivered());
        let s = t.summary();
        assert!(
            s.contains("190 recipient(s), 400 unit(s) across 1 tx(s)"),
            "{s}"
        );
        assert!(s.contains("FAILED"), "{s}");
        assert!(s.contains("UNVERIFIED"), "{s}");
        // Even a fully delivered pass must not claim the money is paid: a
        // transaction in the mempool is paid only once a block includes it.
        let mut ok = SettleTally::default();
        ok.record(ChunkOutcome::Delivered, 2, 35);
        assert!(ok.all_delivered());
        let s = ok.summary();
        assert!(s.contains("NOT paid until they confirm"), "{s}");
        assert!(!s.contains("FAILED"), "{s}");
        assert!(!s.contains("UNVERIFIED"), "{s}");
        // Nothing attempted at all must not print a settlement line.
        assert!(!SettleTally::default().attempted());
    }

    #[test]
    fn an_api_accept_is_not_proof_the_node_holds_the_payout() {
        // /submit/transaction answers ret=0 as soon as the node has validated the
        // transaction and handed it to a background task; the mempool insert
        // result is discarded. So the accept response says nothing, and only the
        // node's own view of the hash resolves it.
        let submit_ok: serde_json::Value =
            serde_json::from_str(r#"{"ret":0,"hash":"ab"}"#).unwrap();
        assert_eq!(find_u64(&submit_ok, "ret"), Some(0));

        let not_found: serde_json::Value =
            serde_json::from_str(r#"{"ret":1,"err":"transaction not found"}"#).unwrap();
        assert_eq!(admission_of(&not_found), Admission::Missing);

        let in_mempool: serde_json::Value =
            serde_json::from_str(r#"{"ret":0,"pending":true}"#).unwrap();
        assert_eq!(admission_of(&in_mempool), Admission::Held);

        let mined: serde_json::Value = serde_json::from_str(r#"{"ret":0,"confirm":9}"#).unwrap();
        assert_eq!(admission_of(&mined), Admission::Held);

        // No answer from the node is NOT a verdict: the payout may be in flight,
        // so it must stay tracked rather than be re-issued (or counted as paid).
        let offline: serde_json::Value =
            serde_json::from_str(r#"{"http_error":"connection refused"}"#).unwrap();
        assert_eq!(admission_of(&offline), Admission::Unresolved);
        let garbled: serde_json::Value = serde_json::from_str(r#"{"ret":0}"#).unwrap();
        assert_eq!(admission_of(&garbled), Admission::Unresolved);
    }

    #[test]
    fn the_share_rate_limiter_never_discards_a_network_block() {
        // At the cap the pool used to answer "busy" before it looked at whether
        // the submission beat the NETWORK target, throwing away a whole block
        // reward to save one entry in a 2-million-entry set.
        assert!(
            share_limiter_rejects(SEEN_CAP, false),
            "shares shed at the cap"
        );
        assert!(
            !share_limiter_rejects(SEEN_CAP, true),
            "a network block is never refused by the share limiter"
        );
        assert!(!share_limiter_rejects(SEEN_CAP - 1, false));
        assert!(!share_limiter_rejects(0, false));
        assert!(!share_limiter_rejects(usize::MAX, true));
    }

    #[test]
    fn no_worker_may_submit_faster_than_it_could_have_hashed() {
        // A miner that sits on its shares has a whole interval's worth to insert
        // at once. Nothing in the submission says when a share was FOUND, so the
        // only handle the pool has is that finding one costs a known number of
        // hashes: past a ceiling, a submission rate is claiming work no hardware
        // on this chain performed.
        // The budget is the ceiling divided by what one share costs: a cheaper
        // share means a worker could genuinely find more of them per second.
        let cost = 30; // one share costs 2^30 hashes
        assert_eq!(
            worker_share_rate(cost),
            1 << (MAX_WORKER_HASHRATE_LOG2 - cost)
        );
        assert!(worker_share_rate(cost - 1) > worker_share_rate(cost));

        // A share so expensive that even the ceiling could not find one per
        // second still leaves every worker able to submit: an honest miner
        // locked out of submitting is an honest miner mining for nothing.
        let per_sec = worker_share_rate(MAX_WORKER_HASHRATE_LOG2 + 20);
        assert_eq!(per_sec, 1);
        let burst = worker_burst(per_sec);
        assert_eq!(burst, WORKER_BURST_MIN_SHARES);
        let mut st = ShareRate {
            shares: burst,
            at_ms: 1_000,
        };
        // The whole burst goes through back to back, and the next one does not:
        // a dump is bounded by what the ceiling could have found.
        for i in 0..burst {
            assert!(rate_admits(&mut st, 1_000, per_sec, burst), "share {i}");
        }
        assert!(
            !rate_admits(&mut st, 1_000, per_sec, burst),
            "an unbounded burst is exactly the withheld batch this stops"
        );
        // It refills with time, so a miner running steadily is never held back.
        assert!(rate_admits(&mut st, 2_000, per_sec, burst));
        assert!(!rate_admits(&mut st, 2_000, per_sec, burst));

        // The same budget on a live pool, keyed per worker: throttling everybody
        // because one miner misbehaves would cost every honest miner its shares.
        let mut p = a_pool();
        // A share target with 71 leading zero bits: one share costs 2^71 hashes,
        // far more than the ceiling finds in a second.
        let mut dear = [0u8; 32];
        dear[8] = 0x01;
        p.share_target = dear;
        assert_eq!(pool_core::share_cost_bits(&p.share_target), 71);
        for i in 0..WORKER_BURST_MIN_SHARES {
            assert!(p.rate_admits_share(W_A, 1_000), "share {i}");
        }
        assert!(
            !p.rate_admits_share(W_A, 1_000),
            "the budget has to bind on the live pool, not only in theory"
        );
        assert!(p.rate_admits_share(W_B, 1_000));
    }

    #[test]
    fn a_burst_of_withheld_shares_cannot_take_an_honest_miners_payout() {
        // End to end through the pool's own accounting, at the point where money
        // is decided. "honest" mines the interval and sends its shares in as it
        // finds them. "hoarder" mines the same interval, sends nothing, and dumps
        // a full window's worth in the second before the settlement tick, which
        // evicts every one of honest's shares from the window.
        let mut p = a_pool();
        let start = 1_000_000u64;
        for i in 0..PPLNS_WINDOW as u64 {
            p.pplns.record(W_A, start + i * 10);
        }
        let tick = start + 120_000;
        for i in 0..PPLNS_WINDOW as u64 {
            p.pplns.record(W_B, tick + i);
        }
        // The headcount the pool used to settle on: the hoarder owns all of it.
        assert_eq!(p.pplns.count_of(W_B), PPLNS_WINDOW as u64);
        assert_eq!(p.pplns.count_of(W_A), 0);

        let now = tick + PPLNS_WINDOW as u64;
        // Exactly what `settle_once` divides the matured balance by.
        let paid: HashMap<String, u64> = plan_settlement(1_000, &settlement_credit(&p, now))
            .into_iter()
            .collect();
        let honest = *paid.get(W_A).unwrap_or(&0);
        let hoarder = *paid.get(W_B).unwrap_or(&0);
        assert!(
            honest > 900 && hoarder < 100,
            "a withheld batch took the payout: honest={honest} hoarder={hoarder}"
        );
        // And /earnings tells the same story it settles on, so a miner is never
        // shown a pending figure the pool has no intention of paying.
        let a = p.earnings_of(W_A, now);
        let b = p.earnings_of(W_B, now);
        assert!(a.credit > b.credit * 9);
        assert_eq!(a.window_credit, b.window_credit);
        assert!(worker_pending_units(1_000, a.credit, a.window_credit) > 900);
        assert!(worker_pending_units(1_000, b.credit, b.window_credit) < 100);
    }

    /* ---- the pool must mine (and serve) the node's transaction set ---- */

    fn a_template(txs: PackedTxs) -> Template {
        Template {
            height: 4321,
            prevhash: Hash::from([0x5au8; 32]),
            timestamp: 1_700_000_000,
            difficulty: 0x2000_0000,
            target: [0xff; 32],
            coinbase_addr: Address::default(),
            txs: Arc::new(txs),
        }
    }

    #[test]
    fn a_worker_is_served_the_merkle_siblings_it_needs() {
        // Regression: the pool hard-coded `"mkrl_modify_list": []`. That was only
        // safe while its blocks were coinbase-only. A standard worker rebuilds
        // the merkle root from its OWN coinbase nonce folded through this list,
        // so serving an empty list for a block that carries transactions makes
        // every share that worker finds hash to something the pool never
        // reconstructs, and be rejected wholesale.
        let siblings = vec![Hash::from([0x11u8; 32]), Hash::from([0x22u8; 32])];
        let tpl = a_template(PackedTxs {
            bodies: vec![vec![0xaa], vec![0xbb], vec![0xcc]],
            mrklrts: siblings.clone(),
        });
        let body: serde_json::Value =
            serde_json::from_str(&pending_cache_json(&tpl, &[0x7f; 32])).expect("json");
        let served: Vec<String> = body["mkrl_modify_list"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|v| v.as_str().expect("hex string").to_string())
            .collect();
        assert_eq!(
            served,
            siblings
                .iter()
                .map(|h| hex::encode(h.serialize()))
                .collect::<Vec<_>>(),
            "the worker must get the same siblings the pool folds through"
        );
        assert_eq!(body["height"].as_u64(), Some(4321));
        // The header the worker is handed must already claim all four
        // transactions, or the node rejects the block it eventually builds.
        let intro = hex::decode(body["block_intro"].as_str().expect("intro")).expect("hex");
        assert_eq!(intro.len(), 89);
        assert_eq!(
            u32::from_be_bytes(intro[75..79].try_into().unwrap()),
            4,
            "coinbase plus the node's three transactions"
        );
        // A template with nothing packed still serves an empty list, exactly as
        // before, so the coinbase-only fallback is unchanged for workers.
        let bare: serde_json::Value = serde_json::from_str(&pending_cache_json(
            &a_template(PackedTxs::default()),
            &[0x7f; 32],
        ))
        .expect("json");
        assert_eq!(
            bare["mkrl_modify_list"].as_array().map(|a| a.len()),
            Some(0)
        );
    }

    #[test]
    fn a_worker_whose_every_share_is_rejected_is_shouted_about() {
        // The pool rebuilds each share's header itself, so a worker that computes
        // a different merkle root is REJECTED, never credited - it cannot steal.
        // But it also cannot earn, and a silent reject counter is how a miner
        // burns a day of hashrate for nothing.
        let mut streaks: HashMap<String, BadStreak> = HashMap::new();
        let t0 = 1_000_000u64;
        for _ in 1..BAD_STREAK_WARN {
            assert_eq!(bump_bad_streak(&mut streaks, "miner-a", t0), None);
        }
        assert_eq!(
            bump_bad_streak(&mut streaks, "miner-a", t0),
            Some(BAD_STREAK_WARN)
        );
        // It keeps saying so, but on a CLOCK now, not on a reject count: see
        // `a_restart_does_not_flood_the_log_with_the_same_streak_line`.
        let later = t0 + BAD_STREAK_REPEAT_MS;
        for _ in 0..BAD_STREAK_WARN {
            assert_eq!(bump_bad_streak(&mut streaks, "miner-a", t0), None);
        }
        assert!(bump_bad_streak(&mut streaks, "miner-a", later).is_some());
        // One accepted share means the two agree again: the streak restarts.
        streaks.remove("miner-a");
        assert_eq!(bump_bad_streak(&mut streaks, "miner-a", later), None);
        // Streaks are per worker, so one broken miner never accuses another.
        assert_eq!(bump_bad_streak(&mut streaks, "miner-b", later), None);
        // Bounded: a flood of invented ids must not grow memory without limit.
        let mut flood: HashMap<String, BadStreak> = (0..BAD_STREAK_WORKERS)
            .map(|i| {
                (
                    format!("w{i}"),
                    BadStreak {
                        count: 1,
                        warned_at_ms: None,
                    },
                )
            })
            .collect();
        assert_eq!(bump_bad_streak(&mut flood, "one-too-many", t0), None);
        assert_eq!(flood.len(), BAD_STREAK_WORKERS);
        // A worker already tracked still counts once the map is full.
        assert!(flood.contains_key("w0"));
        bump_bad_streak(&mut flood, "w0", t0);
        assert_eq!(flood["w0"].count, 2);
    }

    #[test]
    fn a_restart_does_not_flood_the_log_with_the_same_streak_line() {
        // Measured on a rig: a pool restarted inside a height re-stamped its
        // template, so every connected worker went on hashing a header the pool no
        // longer held until its scan pass ended. One worker produced 6,320
        // consecutive rejects and another 5,424, and 1,281 copies of the streak
        // line were written.
        //
        // The old rule shouted at every multiple of BAD_STREAK_WARN, which taxes
        // the reject RATE and not elapsed time: the faster the worker, the louder
        // the pool. What follows is the whole 6,320-reject burst arriving over ten
        // minutes of the pool's clock.
        let mut streaks: HashMap<String, BadStreak> = HashMap::new();
        let t0 = 1_000_000u64;
        let burst = 6_320u64;
        let span_ms = 600_000u64;
        let mut lines = 0usize;
        for i in 0..burst {
            let now = t0 + (i * span_ms) / burst;
            if bump_bad_streak(&mut streaks, "miner-a", now).is_some() {
                lines += 1;
            }
        }
        // Once when the streak is first established, then no more often than
        // BAD_STREAK_REPEAT_MS for as long as it lasts.
        let ceiling = (span_ms / BAD_STREAK_REPEAT_MS + 1) as usize;
        assert!(
            lines <= ceiling,
            "{lines} copies of the streak line for one worker over {span_ms}ms; \
             at most {ceiling} may be printed"
        );
        assert!(lines >= 1, "the streak must still be reported at all");
        assert_eq!(
            streaks["miner-a"].count, burst,
            "every reject is still counted"
        );

        // The clock is what rate limits, so a fleet all hitting this at once is
        // still heard from: one worker's line must not silence another's.
        let mut fleet: HashMap<String, BadStreak> = HashMap::new();
        for w in ["miner-a", "miner-b", "miner-c"] {
            let mut said = false;
            for _ in 0..BAD_STREAK_WARN {
                said |= bump_bad_streak(&mut fleet, w, t0).is_some();
            }
            assert!(said, "{w} was never reported");
        }
    }

    #[test]
    fn a_flood_of_accepted_shares_is_one_line_that_still_says_who_is_mining() {
        // Measured on a rig: one println per accepted share filled 7.6 MB of
        // pool.out.log in four minutes, roughly a thousand lines a second, over
        // 3.3 million shares. The cost is not disk. The block-found notice went
        // out through that SAME println, so the single line that says the pool
        // earned money - or that the node refused the block and the whole reward
        // is gone - was buried in thousands of identical lines a second. An
        // operator who cannot read the log cannot tell a pool that is working
        // from one that is not, and this pool holds other people's money.
        let mut log = ShareLog::default();
        let t0 = 1_700_000_000_000u64;

        // The first accepted share of the process still speaks at once: someone
        // bringing a new pool up has to see that work is arriving, and it says
        // what the following lines will be so the quiet is not read as a stall.
        let first = log.note(W_A, 4321, t0).expect("the first share says so");
        assert!(first.contains("first accepted share"), "{first}");
        assert!(first.contains(W_A), "{first}");
        assert!(first.contains("one summary line every 10s"), "{first}");

        // Then the flood. 20,000 shares inside one summary window - about the
        // rate the rig produced - and stdout stays silent.
        for i in 0..20_000u64 {
            let w = if i % 2 == 0 { W_A } else { W_B };
            let at = t0 + i % SHARE_LOG_EVERY_MS;
            assert_eq!(
                log.note(w, 4321, at),
                None,
                "share {i} took a line of its own"
            );
        }

        // One line covers all of them, and it carries what an operator actually
        // needs: how many, over what stretch, at what height, and from whom. A
        // bare "shares are arriving" would be no better than the flood.
        let line = log
            .note(W_B, 4322, t0 + SHARE_LOG_EVERY_MS)
            .expect("a summary is due");
        assert!(line.contains("20001 accepted"), "{line}");
        assert!(line.contains("in the 10s since the last line"), "{line}");
        assert!(line.contains("height 4322"), "{line}");
        assert!(line.contains("2 workers"), "{line}");
        assert!(line.contains(W_A) && line.contains(W_B), "{line}");

        // The count starts over, so the next line does not re-report the same
        // shares - a summary that double counts is a summary nobody can use.
        assert_eq!(log.note(W_A, 4322, t0 + SHARE_LOG_EVERY_MS + 1), None);
        let next = log
            .due(t0 + 2 * SHARE_LOG_EVERY_MS + 1)
            .expect("the second window is due");
        assert!(next.contains("1 accepted"), "{next}");
        assert!(next.contains("1 worker:"), "{next}");

        // A big fleet is counted, not listed: the set behind the line is grown
        // on the share hot path under the pool lock, so it is bounded, and past
        // the bound the headcount says it is a floor instead of under-reporting
        // the fleet.
        let mut fleet = ShareLog::default();
        fleet.last_ms = Some(t0);
        for i in 0..(SHARE_LOG_WORKERS + 50) {
            assert_eq!(fleet.note(&format!("worker-{i:04}"), 4322, t0), None);
        }
        assert_eq!(fleet.workers.len(), SHARE_LOG_WORKERS, "the set is bounded");
        let big = fleet
            .due(t0 + SHARE_LOG_EVERY_MS)
            .expect("the fleet's line is due");
        assert!(big.contains("at least 64 workers"), "{big}");
        assert!(big.contains("(+60 more)"), "{big}");
        assert!(
            big.contains("worker-0000") && !big.contains("worker-0060"),
            "the line names a readable few and counts the rest: {big}"
        );

        // Nothing accepted means nothing to say: the timer flush must not print
        // "0 accepted" every two seconds at an idle pool.
        let mut idle = ShareLog::default();
        assert_eq!(idle.due(t0), None);
        assert_eq!(idle.due(t0 + 10 * SHARE_LOG_EVERY_MS), None);

        // And a clock that steps BACKWARDS (ntp correction, a VM resumed from a
        // snapshot) must not silence the pool until it catches up.
        let mut stepped = ShareLog::default();
        stepped.last_ms = Some(t0);
        assert_eq!(stepped.note(W_A, 4322, t0 + 1), None);
        let back = stepped
            .note(W_A, 4322, t0 - 3_600_000)
            .expect("a clock step must not mute the pool for an hour");
        assert!(back.contains("2 accepted"), "{back}");
    }

    #[test]
    fn the_share_path_summarises_but_a_found_block_always_announces_itself() {
        // The same fault where it actually bites: on /submit/miner/success, the
        // route an unmodified poworker mines into. It printed one line per
        // accepted share AND the block notice through the same println. These
        // are now separate: shares are summarised, a block is not rate limited,
        // not summarised and not deferred, because it is the one event an
        // operator must never miss.
        let mut p = a_pool();
        // Every hash clears the share target here (that is a_pool's default) but
        // nothing reaches the network target, so these are ordinary shares.
        p.network_target = [0u8; 32];
        let height = p.tpl.height;
        let pool = Arc::new(Mutex::new(p));
        let shares = 16u32;
        for n in 0..shares {
            let r = handle_submission(&pool, W_A, height, [0x22u8; 32], n);
            assert_eq!(r["kind"].as_str(), Some("share"), "{r}");
        }
        {
            let g = plock(&pool);
            assert_eq!(g.accepted, u64::from(shares), "every share is still credited");
            assert!(
                g.share_log.pending > 0,
                "the share path is not folding anything into a summary: it is back to \
                 one stdout line per accepted share"
            );
            assert!(
                g.share_log.workers.contains(W_A),
                "the summary has to be able to name who is mining"
            );
        }

        // Now one that beats the network target. It is a whole block reward.
        {
            let mut g = plock(&pool);
            g.network_target = [0xff; 32];
        }
        let before = plock(&pool).share_log.pending;
        let r = handle_submission(&pool, W_A, height, [0x22u8; 32], shares);
        assert_eq!(r["kind"].as_str(), Some("block"), "{r}");
        let g = plock(&pool);
        assert_eq!(
            g.share_log.pending, before,
            "a block must not be folded into the share summary: folded means deferred, \
             and a deferred block notice is one an operator reads minutes late or, if \
             the miners then go quiet, never"
        );
        assert_eq!(
            g.submitted.len(),
            1,
            "and it is still counted as a submitted block"
        );
    }

    #[test]
    fn the_block_notice_says_which_worker_found_what() {
        // It is printed BEFORE the block is submitted, so a submit that hangs or
        // is refused still leaves the operator knowing a block was found here,
        // by whom, and at what height - the three things needed to check whether
        // the reward ever landed.
        let line = block_found_line(W_A, 4321, &[0xabu8; 32]);
        assert!(line.starts_with("[block] SOLVED height 4321"), "{line}");
        assert!(line.contains(W_A), "{line}");
        assert!(line.contains(&hex::encode([0xabu8; 32])), "{line}");
        // Not tagged like a share: an operator greps for one or the other.
        assert!(!line.contains("[shares]"), "{line}");
    }

    #[test]
    fn the_streak_line_never_states_a_worker_fault_the_pool_cannot_know() {
        // The old line ended "Nothing this worker submits can be credited until
        // that is fixed" - a permanent worker-side fault, stated as fact. A pool
        // restart mid-height causes exactly the same symptom, transiently, and on
        // the rig that sentence was printed 1,281 times about workers that were
        // behaving correctly and recovered on their own. An operator who believes
        // it rebuilds a miner that was never broken while the real state - the
        // pool's own re-stamped header - goes unexamined.
        let banned = [
            "Nothing this worker submits can be credited",
            "until that is fixed",
        ];
        // Just after a template change or a restart, which is when the pool itself
        // is the likeliest cause.
        let fresh = bad_streak_message("miner-a", 6_320, 12_000);
        // ...and long after, when a worker on a stale header no longer explains it.
        let old = bad_streak_message("miner-a", 6_320, TEMPLATE_SETTLE_MS + 60_000);
        for msg in [&fresh, &old] {
            for phrase in banned {
                assert!(
                    !msg.contains(phrase),
                    "the pool cannot know this and must not assert it: {msg}"
                );
            }
            // What it may say is what it saw, and who it saw it from.
            assert!(msg.contains("miner-a") && msg.contains("6320"));
            assert!(
                msg.contains("Observed, not diagnosed"),
                "the line must be framed as an observation: {msg}"
            );
            // Every line carries how long ago the pool last moved the header, so
            // the operator can weigh it without taking the pool's word for it.
            assert!(msg.contains("changed the header it serves"));
        }
        // Inside the settle window the line says the pool caused it and that it
        // clears by itself; outside, it points at the worker as the NEXT thing to
        // check rather than as a finding.
        assert!(fresh.contains("just restarted") && fresh.contains("Nothing to do yet"));
        assert!(!old.contains("Nothing to do yet"));
        assert!(old.contains("Worth checking next"));
    }

    #[test]
    fn the_header_timestamp_survives_a_restart_through_the_state_file() {
        // The stamp lives in the 89-byte header every worker hashes, and the pool
        // pins one template per height. Without it on disk a restart inside a
        // height invents a new stamp, so the pool serves a DIFFERENT header for the
        // SAME height while /query/miner/notice - which signals only a height
        // change - stays quiet. Every worker keeps hashing the dead header until
        // its scan pass ends and earns nothing for it.
        let mut path = std::env::temp_dir();
        path.push(format!("hbit-pool-stamp-pin-{}", std::process::id()));
        let path = path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&path);

        let mut p = a_pool();
        p.state_file = path.clone();
        let shot = p.state_shot(true).expect("a snapshot");
        std::fs::write(&path, &shot.body).expect("write the state file");

        let pin = read_stamp_pin(&path).expect("the state file must carry the header stamp");
        assert_eq!(pin.height, p.tpl.height);
        assert_eq!(pin.prevhash, p.tpl.prevhash);
        assert_eq!(pin.timestamp, p.tpl.timestamp);

        // And it is the pin the next run actually mines on: same height, same
        // parent, so the header comes back byte for byte.
        assert_eq!(
            hbit_pool::template_timestamp(
                Some(&pin),
                p.tpl.height,
                &p.tpl.prevhash,
                p.tpl.timestamp - 30,
                p.tpl.timestamp + 68,
            ),
            p.tpl.timestamp
        );

        // A state file from a build that never wrote the stamp is not an error: the
        // pool stamps a fresh template exactly as it always did.
        let older = serde_json::json!({"accepted": 7});
        assert_eq!(parse_stamp_pin(&older), None);
        // Nor is a half-written one. Anything short of all three fields is refused
        // rather than guessed at - a wrong stamp goes into a real block.
        let partial = serde_json::json!({"template_stamp": {"height": 350, "timestamp": 1}});
        assert_eq!(parse_stamp_pin(&partial), None);
        let unreadable = serde_json::json!({"template_stamp": {
            "height": 350, "timestamp": 1, "prevhash": "not hex",
        }});
        assert_eq!(parse_stamp_pin(&unreadable), None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_block_the_chain_never_reaches_is_reported_once() {
        // /submit/block validates asynchronously and answers before the verdict,
        // so a refused block leaves no trace but a tip that never gets there. On
        // a chain where this pool is the only miner that is total silence.
        let mut state: HashMap<(u64, [u8; 32]), u32> = HashMap::new();
        let ours = (100u64, [7u8; 32]);
        let pending = [ours];
        // No tip this cycle (node unreachable): never accuse it.
        for _ in 0..BLOCK_STALL_CYCLES * 2 {
            assert!(note_block_stalls(&mut state, &pending, None).is_empty());
        }
        assert!(state.is_empty());
        for _ in 1..BLOCK_STALL_CYCLES {
            assert!(note_block_stalls(&mut state, &pending, Some(99)).is_empty());
        }
        assert_eq!(
            note_block_stalls(&mut state, &pending, Some(99)),
            vec![ours],
            "the chain has not reached our height for far too long"
        );
        // Exactly once: it must not repeat every two seconds afterwards.
        for _ in 0..BLOCK_STALL_CYCLES {
            assert!(note_block_stalls(&mut state, &pending, Some(99)).is_empty());
        }
        // The chain reaching that height clears it, whatever block landed there
        // (accepted, or orphaned - the confirm tally reports that separately).
        assert!(note_block_stalls(&mut state, &pending, Some(100)).is_empty());
        assert!(state.is_empty());
        // A block the pool stopped tracking is dropped rather than leaked.
        note_block_stalls(&mut state, &pending, Some(99));
        assert_eq!(state.len(), 1);
        note_block_stalls(&mut state, &[], Some(99));
        assert!(state.is_empty());
    }

    #[test]
    fn a_refused_block_submission_is_recognised_as_a_refusal() {
        // A refusal costs a whole block reward, and it used to reach nobody but
        // the winning worker's JSON response.
        assert!(!block_submit_refused(r#"{"ret":0,"ok":true}"#));
        assert!(block_submit_refused(
            r#"{"ret":1,"err":"block parse failed"}"#
        ));
        assert!(block_submit_refused("http_error: connection refused"));
        assert!(block_submit_refused("<html>502 Bad Gateway</html>"));
        assert!(block_submit_refused(""));
    }

    #[test]
    fn the_empty_block_warning_repeats_without_flooding() {
        // The template loop runs every couple of seconds. The operator has to be
        // told, and has to keep being told, without the warning becoming the
        // entire log.
        let mut state: Option<(String, Instant)> = None;
        let t0 = Instant::now();
        assert!(should_warn_now(&mut state, "miner not enabled", t0));
        assert!(!should_warn_now(&mut state, "miner not enabled", t0));
        assert!(!should_warn_now(
            &mut state,
            "miner not enabled",
            t0 + TX_WARN_REPEAT - Duration::from_secs(1)
        ));
        assert!(should_warn_now(
            &mut state,
            "miner not enabled",
            t0 + TX_WARN_REPEAT
        ));
        // A different reason is news, whenever it arrives.
        assert!(should_warn_now(
            &mut state,
            "packing another height",
            t0 + TX_WARN_REPEAT
        ));
        assert!(!should_warn_now(
            &mut state,
            "packing another height",
            t0 + TX_WARN_REPEAT
        ));
    }

    /// One immature block, with the fees uncounted as `submit_share` records it.
    fn an_immature_block(height: u64, hash: u8) -> Immature {
        Immature {
            height,
            hash: [hash; 32],
            units: block_reward_units(height),
            fees_counted: false,
        }
    }

    #[test]
    fn immature_block_income_is_not_distributable() {
        // Two found blocks are still shallow, so their subsidy must stay out of
        // the payout even though the node already credited it to the wallet.
        let immature = [an_immature_block(900, 1), an_immature_block(901, 2)];
        let held: u64 = immature.iter().map(|e| e.units).sum();
        assert!(held > 0);
        let reserve = 5u64;
        // Balance is exactly the two fresh subsidies plus the reserve: nothing
        // has matured, so the pool must pay nothing at all.
        assert_eq!(distributable_units(held + reserve, held, reserve), None);
        // Once one of them buries, only that block's income is released.
        let matured_one = immature[0].units;
        assert_eq!(
            distributable_units(held + reserve, held - matured_one, reserve),
            Some(matured_one)
        );
    }

    #[test]
    fn the_pool_server_restarts_onto_a_state_file_the_previous_build_wrote() {
        // THE upgrade path: an operator stops the shipped pool and starts this
        // one on the file it left behind. That file is the only record of who is
        // owed what, and TWO of this release's fixes changed its shape at once -
        // the share window gained arrival times, the settlement ledger gained an
        // owed list - each written believing it was the only reader. This is that
        // file, in the exact shape `state_shot` and `PayoutRecord::to_json` used
        // to emit, loaded by the code that replaces them.
        let mut path = std::env::temp_dir();
        path.push(format!("hbit-pool-restart-{}.state.json", std::process::id()));
        let path = path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&path);
        let old = json!({
            "window": PPLNS_WINDOW,
            // Bare worker ids: no arrival times anywhere.
            "order": [W_A, W_B, W_A],
            "accepted": 3,
            "blocks": 2,
            "orphaned": 1,
            "settle_pending_txs": ["aa11"],
            // No `body_hex`, because that build kept none.
            "payouts_inflight": [{
                "hash": "aa11",
                "at": 1_700_000_000u64,
                "node_holds": true,
                "rows": [[W_A, 12], [W_B, 3]],
            }],
            "paid": {
                "since": 1_600_000_000u64,
                "rows": [{
                    "worker": W_B, "units": 41, "last_units": 9,
                    "last_hash": "bb22", "last_at": 1_650_000_000u64,
                }],
            },
            "submitted": [{"height": 901, "hash": "cd".repeat(32)}],
            // No `fees_counted`, because that build held back the subsidy alone.
            "immature": [{"height": 900, "hash": "ab".repeat(32), "units": 30}],
            // ...and no `owed` key at all.
        });
        std::fs::write(&path, old.to_string()).expect("write the old state file");

        let mut p = a_pool();
        p.state_file = path.clone();
        p.load_state();

        // The window comes back whole, and weighing what the old build weighed
        // it at: 2 shares to 1, which is the split that build would have paid.
        assert_eq!(p.pplns.total(), 3);
        assert_eq!(p.pplns.count_of(W_A), 2);
        assert_eq!(p.pplns.count_of(W_B), 1);
        let now = pool_core::now_ms();
        let (a, total) = p.pplns.credit_share(W_A, now);
        let (b, _) = p.pplns.credit_share(W_B, now);
        assert!(a > 0 && b > 0, "an upgraded window must credit somebody");
        assert_eq!(a, 2 * b, "the upgrade moved money between miners");
        assert_eq!(total, a + b);

        // The payout in flight survives with its rows, and with NO bytes - which
        // must read as "cannot be put back on the wire", never as permission to
        // re-sign the window.
        assert_eq!(p.payout_records.len(), 1);
        assert!(p.payout_records[0].body_hex.is_empty());
        assert_eq!(p.inflight_units, 15);
        assert_eq!(p.settle_pending_txs, vec!["aa11".to_string()]);
        assert_eq!(
            gone_action(Some(&p.payout_records[0])),
            GoneAction::Stuck,
            "a legacy payout the node held must freeze settlement, not be re-issued"
        );

        // A file with no owed list is an empty debt, not a corrupt file.
        assert!(p.owed.is_empty());

        // Paid history, blocks awaiting confirmation and the maturity hold-back
        // all survive, and the hold-back reads as fees-NOT-counted: that file
        // really does carry the subsidy alone, and reading it the other way
        // would distribute those fees on the first settlement after the upgrade.
        assert_eq!(p.paid.since, 1_600_000_000);
        assert_eq!(p.paid.get(W_B).expect("W_B").units, 41);
        assert_eq!(p.blocks, 2);
        assert_eq!(p.orphaned, 1);
        assert_eq!(p.submitted.len(), 1);
        assert_eq!(
            p.immature,
            vec![Immature {
                height: 900,
                hash: [0xab; 32],
                units: 30,
                fees_counted: false,
            }]
        );

        // /earnings answers about both miners out of that file without a node.
        let e = p.earnings_of(W_A, now);
        assert!(e.known && e.shares == 2 && e.inflight_units == 12);
        assert_eq!(e.owed_units, 0);

        // And what it writes back is the NEW shape, whole: the next restart is an
        // ordinary one.
        let shot = p.state_shot(true).expect("a snapshot");
        let back: serde_json::Value = serde_json::from_slice(&shot.body).expect("json");
        assert!(
            back["order"][0].is_array(),
            "the rewrite must carry arrival times: {}",
            back["order"]
        );
        assert_eq!(back["owed"].as_array().map(|a| a.len()), Some(0));
        assert_eq!(back["immature"][0]["fees_counted"].as_bool(), Some(false));
        assert_eq!(back["credit_horizon_ms"].as_u64(), Some(p.pplns.horizon_ms()));
        assert_eq!(back["paid"]["rows"][0]["units"].as_u64(), Some(41));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_found_blocks_transaction_fees_are_held_back_with_its_subsidy() {
        // The pool packs the node's transactions, and the chain credits the sum
        // of their fees to the coinbase address - the same wallet that settles.
        // A hold-back of the subsidy alone leaves that fee income spendable at
        // ZERO confirmations, and if the block is then orphaned the money is
        // gone from the chain while the payout that spent it is still valid: the
        // operator funds the difference out of their own pocket.
        let mut immature = vec![an_immature_block(900, 1)];
        let subsidy = immature[0].units;
        let fees = 7u64;
        let reserve = 5u64;
        // The wallet holds exactly this one block's whole income plus the fee
        // reserve, and the block is one confirmation deep.
        let balance = subsidy + fees + reserve;

        // Before the fees are counted the hold-back is short by exactly them,
        // and that shortfall is what a settlement would pay out.
        let held: u64 = immature.iter().map(|e| e.units).sum();
        assert_eq!(distributable_units(balance, held, reserve), Some(fees));

        // Counting them closes it: nothing at all is distributable until the
        // chain buries the block.
        assert!(fold_block_fees(&mut immature, 900, &[1u8; 32], fees));
        let held: u64 = immature.iter().map(|e| e.units).sum();
        assert_eq!(distributable_units(balance, held, reserve), None);
        assert_eq!(immature[0].units, subsidy + fees);
        assert!(immature[0].fees_counted);

        // The node is re-read every settlement cycle until it answers, so the
        // same fees must never be folded in twice: that would hold back money
        // the wallet does not contain and stop paying the miners it belongs to.
        assert!(!fold_block_fees(&mut immature, 900, &[1u8; 32], fees));
        assert_eq!(immature[0].units, subsidy + fees);
        // And a block we did NOT find at that height is not ours to fold into.
        assert!(!fold_block_fees(&mut immature, 900, &[9u8; 32], fees));
        assert!(!fold_block_fees(&mut immature, 901, &[1u8; 32], fees));
        assert_eq!(immature[0].units, subsidy + fees);
    }

    /* ---- the operator's first ten minutes ---- */

    #[test]
    fn the_usage_text_is_enough_to_start_the_pool_without_reading_the_source() {
        let u = usage();
        // Every argument is named and explained, in the order they are typed.
        for arg in [
            "<node>",
            "<wallet_file>",
            "<listen>",
            "<share_bits>",
            "<chain>",
            "[settle_secs]",
        ] {
            assert!(u.contains(arg), "usage never explains {arg}:\n{u}");
        }
        // A command that really works, with the required arguments in place.
        assert!(
            u.contains(
                "hbit-pool-server http://127.0.0.1:8080 pool-wallet.key 0.0.0.0:9777 24 mainnet"
            ),
            "usage has no working example:\n{u}"
        );
        // The bounds and defaults are read from the constants that enforce them,
        // so the help can never describe a pool that does not exist.
        for n in [
            MIN_SHARE_FACTOR.to_string(),
            MAX_SHARE_FACTOR.to_string(),
            DEFAULT_SHARE_BITS.to_string(),
            MIN_SETTLE_SECS.to_string(),
            MAX_SETTLE_SECS.to_string(),
            DEFAULT_SETTLE_SECS.to_string(),
        ] {
            assert!(u.contains(&n), "usage never mentions {n}:\n{u}");
        }
        // How to protect the key, and how a miner actually joins.
        assert!(
            u.contains(WALLET_PASSWORD_ENV) && u.contains(WALLET_PASSWORD_FILE_ENV),
            "{u}"
        );
        assert!(
            u.contains("connect =") && u.contains("pool_worker ="),
            "{u}"
        );
        // And it ships nothing anybody could paste and pay: no address-shaped
        // string, no key-shaped string, no passphrase. An earlier audit found a
        // shipped file carrying a live third-party address; help text is exactly
        // the kind of place that happens again.
        for line in u.lines() {
            for word in line.split_whitespace() {
                let w = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
                assert!(
                    !is_payout_address(w),
                    "usage carries something payable: {word}"
                );
                assert!(
                    !(w.len() == 64 && w.chars().all(|c| c.is_ascii_hexdigit())),
                    "usage carries something key-shaped: {word}"
                );
            }
        }
    }

    #[test]
    fn the_startup_summary_states_the_same_terms_the_pool_enforces() {
        // The console readback and /terms must never tell a miner two different
        // stories, so both are built from the same constants.
        let t = terms_json(
            PPLNS_WINDOW as u64,
            pplns_horizon_ms(DEFAULT_SETTLE_SECS),
            24,
            24,
            16,
            None,
            0x2000_0000,
            DEFAULT_SETTLE_SECS,
            COINBASE_MATURITY_DEPTH,
            PAYOUT_MATURITY_DEPTH,
        );
        let s = startup_summary(
            W_A,
            "pool-wallet.key",
            true,
            "http://127.0.0.1:8080",
            "mainnet",
            Some(738_700),
            "0.0.0.0:9777",
            PPLNS_WINDOW as u64,
            DEFAULT_SETTLE_SECS,
            24,
            24,
        );
        // The four things an operator has to be able to check.
        assert!(s.contains(W_A), "the address it pays FROM is missing:\n{s}");
        assert!(s.contains("pool-wallet.key"), "{s}");
        assert!(s.contains("http://127.0.0.1:8080"), "{s}");
        assert!(s.contains("mainnet") && s.contains("738700"), "{s}");
        assert!(s.contains("9777") && s.contains("pool_worker"), "{s}");
        // The terms, matching the endpoint exactly.
        assert!(
            s.contains(&t["window_shares"].as_u64().unwrap().to_string()),
            "{s}"
        );
        assert!(
            s.contains(&hac(t["minimum_payout"]["units"].as_u64().unwrap())),
            "{s}"
        );
        assert!(
            s.contains(&t["coinbase_maturity_blocks"].as_u64().unwrap().to_string()),
            "{s}"
        );
        assert_eq!(
            t["fee"]["units"].as_u64(),
            Some(0),
            "a pool with a fee must say so in the summary too"
        );
        assert!(s.contains("no pool fee"), "{s}");
        // A capped share factor is stated as the number really served, and the
        // number asked for is not passed off as the truth.
        let capped = startup_summary(
            W_A,
            "pool-wallet.key",
            false,
            "http://127.0.0.1:8080",
            "mainnet",
            None,
            "0.0.0.0:9777",
            PPLNS_WINDOW as u64,
            DEFAULT_SETTLE_SECS,
            24,
            21,
        );
        assert!(capped.contains("2^21"), "{capped}");
        assert!(capped.contains("share_bits=24 was asked for"), "{capped}");
        assert!(
            capped.contains("PLAINTEXT"),
            "an unprotected key must be said out loud:\n{capped}"
        );
        assert!(
            capped.contains("height unknown"),
            "a tip that was not read is not a number:\n{capped}"
        );
    }

    #[test]
    fn a_miner_is_never_told_to_connect_somewhere_that_cannot_answer() {
        // A wildcard bind carries no host anybody can dial. Handing 0.0.0.0 to a
        // miner sends it nowhere, and all the miner sees is "cannot connect".
        for wildcard in ["0.0.0.0:9777", ":::9777", "[::]:9777"] {
            let (connect, note) = miner_connect(wildcard);
            assert!(!connect.starts_with("0.0.0.0"), "{connect}");
            assert!(!connect.starts_with("::"), "{connect}");
            assert!(connect.ends_with(":9777"), "{connect}");
            assert!(note.contains("any machine"), "{note}");
        }
        // Loopback is a real address, and the note is the whole point: it is why
        // nobody else can mine here.
        for local in ["127.0.0.1:9777", "localhost:9777", "[::1]:9777"] {
            let (connect, note) = miner_connect(local);
            assert!(connect.ends_with(":9777"), "{connect}");
            assert!(note.contains("loopback only"), "{note}");
        }
        // Anything else is handed over as typed.
        let (connect, _) = miner_connect("10.0.0.4:9777");
        assert_eq!(connect, "10.0.0.4:9777");
    }

    #[test]
    fn the_new_wallet_banner_says_what_losing_the_file_costs() {
        let plain = new_wallet_banner("pool-wallet.key", W_A, false);
        assert!(plain.contains("pool-wallet.key"), "{plain}");
        assert!(plain.contains(W_A), "{plain}");
        assert!(
            plain.contains(&pool_state_path("pool-wallet.key")),
            "{plain}"
        );
        // The cost of losing it, in words, not a hint.
        assert!(plain.contains("gone permanently"), "{plain}");
        assert!(plain.contains("BACK IT UP"), "{plain}");
        // With no passphrase the key is lying on the disk, and that is said
        // plainly along with the way out of it.
        assert!(plain.contains("PLAINTEXT"), "{plain}");
        assert!(plain.contains(WALLET_PASSWORD_ENV), "{plain}");
        assert!(plain.contains(&WALLET_PASSWORD_MIN.to_string()), "{plain}");

        // With one, both halves are needed and both must be backed up.
        let enc = new_wallet_banner("pool-wallet.key", W_A, true);
        assert!(enc.contains("ENCRYPTED"), "{enc}");
        assert!(enc.contains("BOTH"), "{enc}");
        assert!(enc.contains("no reset"), "{enc}");
        assert!(!enc.contains("PLAINTEXT"), "{enc}");
    }

    #[test]
    fn a_mistyped_number_is_never_quietly_replaced_by_a_default() {
        // `share_bits` and `settle_secs` used to fall back to their defaults on
        // ANY unparseable value, so an operator who typed one wrong ran for weeks
        // on a setting they had not chosen and were never told about. Both are
        // now parsed strictly and bounded; these are the exact predicates main()
        // applies.
        assert!("twentyfour".trim().parse::<u32>().is_err());
        assert!(
            "24 ".trim().parse::<u32>().is_ok(),
            "surrounding space is not a typo"
        );
        assert!("-1".trim().parse::<u32>().is_err());
        assert!("5m".trim().parse::<u64>().is_err());

        // A settle interval of 0 would spin the settlement thread with no sleep
        // at all, hammering the node forever.
        assert!(!(MIN_SETTLE_SECS..=MAX_SETTLE_SECS).contains(&0));
        assert!((MIN_SETTLE_SECS..=MAX_SETTLE_SECS).contains(&DEFAULT_SETTLE_SECS));
        assert!(check_share_factor(DEFAULT_SHARE_BITS).is_ok());
        // Both refusals name the value to use instead of just the rule.
        let e = check_share_factor(MIN_SHARE_FACTOR - 1).expect_err("refused");
        assert!(e.contains("What to do"), "{e}");
        assert!(e.contains(&DEFAULT_SHARE_BITS.to_string()), "{e}");
    }

    #[test]
    fn money_in_the_startup_text_is_exact() {
        // The console prints tenths of a HAC, which is exactly what a unit is, so
        // this converts rather than rounds. Anything else would put a number on
        // the screen the pool would not pay.
        assert_eq!(hac(0), "0.0 HAC");
        assert_eq!(hac(PAYOUT_DUST_UNITS), "0.1 HAC");
        assert_eq!(hac(SETTLE_RESERVE_UNITS), "0.5 HAC");
        assert_eq!(hac(10), "1.0 HAC");
        assert_eq!(hac(1234), "123.4 HAC");
        // Round trip: every rendering names exactly the units it was given.
        for u in [0u64, 1, 5, 9, 10, 99, 100, 1_000_007] {
            let s = hac(u);
            let digits: String = s
                .trim_end_matches(" HAC")
                .chars()
                .filter(|c| *c != '.')
                .collect();
            assert_eq!(digits.parse::<u64>().expect("digits"), u, "{s}");
        }
        // And the interval reads the way an operator would say it.
        assert_eq!(every(DEFAULT_SETTLE_SECS), "5m");
        assert_eq!(every(30), "30s");
        assert_eq!(every(90), "90s");
        assert_eq!(every(3600), "1h");
    }
}
