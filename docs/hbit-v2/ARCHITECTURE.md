# HBIT pool: how a share becomes money

This describes what the code does **today**, not what it should do. Where a
behaviour is a known defect it says so and does not soften it. Every claim here
was read out of the source; anything that could not be verified is marked as
such rather than guessed.

Nothing in this directory documents a feature that does not exist. Several
documents the upgrade plan calls for are deliberately absent, and
[the index](README.md) says which and why.

## Where state actually lives

Three durable stores and one in-memory authority.

**The in-memory authority** is one process-global `Mutex<Pool>`, taken through
`plock`. Every route, the template thread, the confirm thread and the settlement
thread serialize on it. All accounting lives on that struct: the PPLNS window,
the immature hold-backs, the owed rows, the payout records, the pending payout
hashes and the replay set.

**The accounting file** is `{wallet_file}.state.json`, written by
`atomic_write`: temp file, `sync_all()` when durable, rename, then a parent
directory fsync. The directory fsync is a real fsync on unix and a no-op on
every other platform, Windows included. So on Windows the strongest available
claim is "fsynced bytes, journalled rename", not "durable", and no part of this
system should be described as durable on that platform.

**The wallet key file**, optionally encrypted with Argon2id and AES-256-GCM.

**The settlement lock**, `{wallet_file}.settle.lock`, held with a real OS
exclusive advisory lock. The OS releases it when the holder dies, so a crash
cannot wedge payouts, and deleting the lock file does not free the lock.

The chain is not one of these stores. It is queried, but nothing about who
earned what is ever written to it or derived from it.

## The path, end to end

1. **Template.** The pool reads the node's tip, then that block's intro, and
   builds a template for `tip + 1`. The coinbase always pays the **pool's**
   address, never a worker's. One template is cached per height and served
   byte-identically to every miner.

2. **Work.** `GET /query/miner/pending` returns that cached blob. It ignores
   every query parameter, including `worker`. The `target_hash` it carries is
   the pool's **share** target, so a pooled miner never sees the network target.

3. **Submission.** `GET /submit/miner/success` with height, coinbase nonce,
   block nonce and worker. The worker is whatever the query string says, as long
   as it parses as a payable address. There is no TLS on this path and no header
   is ever read.

4. **Validation**, in four deliberate phases:
   - *lock*: stale height, then the replay set keyed on
     `(height, coinbase_nonce, block_nonce)`. That key has **no worker
     component**, which is defect A9.
   - *no lock*: rebuild the coinbase and the 89-byte header from the pool's own
     template plus the submitted nonces, and hash that. A miner cannot get
     credit for a header the pool did not build, and cannot redirect the reward.
     Two comparisons follow: against the share target, and against the network
     target.
   - *lock*: halt gates, rate limits, replay insert, and then the one line that
     turns hashing into money, `pplns.record(worker, at_ms)`. A found block also
     pushes an `Immature` hold-back row.
   - *no lock*: persist, then serialize and submit the block.

5. **PPLNS.** A 4096-entry deque of `(worker, arrival_ms)`. Credit is
   **milliseconds of residence** in the window, capped at a horizon. Shares
   evicted from the window bank what they earned into time-stamped buckets that
   expire one horizon later. **No share stores the target it was found against**,
   so every share weighs the same regardless of the work it represents.

6. **Maturity.** A background loop asks the node for each tracked height.
   Confirmed means our hash is still there **and** the tip is at least 16 blocks
   past it. The orphan arm is not depth-gated, which is defect A4.

7. **Fees.** Each packed transaction's fee is read back off the node and folded
   into the hold-back once. Any unreadable answer aborts the whole settlement
   cycle rather than valuing the block at zero.

8. **Settlement.** Value the wallet, subtract the hold-backs and a reserve, pay
   old debts off the top, split the rest by **the live PPLNS credit at that
   instant**, merge duplicate rows, chunk at 190 recipients, sign, persist the
   hash and the exact signed bytes and the rows durably, and only then submit.

9. **Paid.** Only a payout buried at least 6 blocks moves a unit from in-flight
   to paid.

## The structural fact the whole upgrade turns on

Between step 4, where a block is found, and step 8, where money is split,
**nothing records who was mining**. The block creates exactly one row, the
hold-back, and no miner list. The income becomes anonymous wallet balance and is
split roughly eighty minutes later over whoever holds credit at that moment.

A miner who connects after a block is found is paid from it. A miner who leaves
before settlement is paid nothing for the work that found it. That is defect A1,
and it is an economic defect rather than a bug: the code does exactly what it
was written to do.

## What is already right, and must survive any change

These were verified by reading, and each exists because something went wrong
once. Anything that breaks one of them is a regression however good it looks.

- **The exclusive settlement lock**, a real OS lock rather than a file whose
  existence means something.
- **Persist before submit.** The signed bytes reach disk and are fsynced before
  the transaction is posted, and a failed write aborts the chunk.
- **Rebroadcast identical bytes, never re-sign.** The decision keys on whether
  the bytes are still held, not on whether the node says it has the transaction.
  A lost acknowledgement is Unresolved, never Rejected.
- **Burial before paid.** Nothing is called paid until the chain has buried it.
- **The 16 block coinbase maturity hold-back**, subtracted before anything is
  split.
- **No floating point anywhere in the money path.** Two `f64` occurrences exist
  in the whole crate and both are inside a test module. Payout splitting is
  integer largest-remainder with a `u128` intermediate, and `overflow-checks`
  is switched on for this package specifically.
- **The pool never trusts a worker-supplied header.**
- **A submission that beats the network target is exempt from every shedding
  rule**: the halt, the per-height cap and the per-worker budget. It is a whole
  block reward and it is never thrown away to save memory.
- **The four phase lock discipline**: the expensive hash happens with the pool
  mutex released.

## The halts

Three, read through one accessor, `Pool::halt_reason()`, in this order.

| halt | derived from | heals by itself |
|---|---|---|
| `accounting_halt` | a durable write that failed | **no** |
| `node_halt` | the tip's own timestamp, every template cycle | yes |
| `share_halt` | the live difficulty, every template change | yes |

A halt stops two things: crediting new shares, and planning fresh payouts. It
deliberately does **not** stop the resolution of payouts already in flight,
which is money owed to named miners and must still reach them, and it does not
stop a found block being accepted, which is the operator's own reward.

One accessor rather than three checks, so a fourth call site cannot be added
that consults only one of them.
