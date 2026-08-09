# What this pool refuses to do

Fail-closed behaviour, as implemented. Every rule below is in the code and has a
test that fails if the rule is removed. Rules that are planned but not written
are in the last section, named as absent.

## It refuses to start against a node it cannot identify

Every other startup check asks the node about its own tip and verifies the
answer is self-consistent: the tip's difficulty really does follow from the
block before it. A node on a different chain passes all of them effortlessly,
because it is perfectly consistent with itself.

So the first question asked is the only one another chain cannot answer the same
way: **where does this chain begin**. The expected hash comes from
`mint::genesis`, through `hbit_pool::mainnet_genesis_hex()`, and is never written
as a literal in the pool. A literal typed from memory into a second file is how
software ends up verifying itself against its own mistake.

It is read from **block 1**, whose `prevhash` is the genesis hash by
construction. Not from block 0: this node does not serve the genesis block at
all. `?height=0` answers "cannot find block", and so does a lookup by its hash,
because the handler defaults `height` to 0 and cannot tell zero from "no height
given". The first version asked for block 0 and had seven passing tests, all
against a stub that answered a shape the real node never produces. Pointing the
pool at a live mainnet node is what found it.

A node that will not answer for block 1 is refused too. Unknown is not
permission.

This applies on mainnet only. A testnet genesis is whatever the person who
started that chain made it, so there is nothing to verify against and pretending
otherwise would refuse every legitimate testnet.

## It refuses to start against a node that stopped following the chain

Right chain, wrong place on it. A node stalled part-way through a sync answers
everything confidently. A pool that starts against one mines a fork nobody else
is on, watches its own blocks get buried sixteen deep **there**, releases the
hold-back on that evidence and signs real payouts against income the real chain
never credited. Miners burn real power for shares that can never mature.

The evidence available is the tip's own timestamp. At startup a tip older than
**3600 seconds** is refused.

That bound is measured rather than assumed. Over the 200 blocks ending at height
771596 the median gap was 212s, the mean 320s against a 300s target, and the
largest 2013s. One gap in two hundred exceeded 1800s, which was the original
bound - and the first live restart of this pool hit exactly that: a healthy node,
a 32 minute gap, and a refusal to start. A one-in-two-hundred chance of telling
an operator their node is broken when it is not becomes a restart loop under
systemd.

## It halts a running pool whose node goes quiet

The same question asked continuously, from the tip's timestamp on every template
cycle, with a much looser bound: **7200 seconds**.

The two bounds differ on purpose. Mainnet aims at one block per 300 seconds and
block arrivals are Poisson, so long gaps happen by chance:

| bound | targets | exceeded in 200 measured mainnet blocks |
|---|---|---|
| 1800s | 6 | 1 of 200 - too tight, and it fired on a healthy node |
| 3600s | 12 | 0 of 200 |
| 7200s | 24 | 0 of 200 |

A false halt on a running pool stops crediting miners who are hashing a template
that is still perfectly valid, so it takes the looser bound. Neither bound can
tell "the chain is quiet" from "this node is stuck": from one node those look
identical, and the threshold is the whole of the answer available.

This check is computed **outside** the "the template changed" branch. The entire
signature of a node that has stopped following the chain is that nothing
changes: it keeps answering, keeps returning the same height, and looks calm. A
check that only runs on a change never runs again.

The template's own timestamp cannot serve here. It is derived from the wall
clock, so on a node stuck a week ago it still reads as now. The tip's timestamp
is the chain's own last heartbeat, and it is carried on the template as
`prev_timestamp` for exactly this reason.

A tip dated in the future counts as zero seconds behind. That is clock skew, and
no pool should stop paying anyone over an ntp correction.

## It never reports an unreadable answer as a lost block

`/submit/block` used to be read as a bool: `ret:0` or everything else. A timeout,
a proxy's HTML error page and an empty body all fell into "everything else" and
were announced to the operator as a whole block reward lost, when the node may
never have seen the bytes.

Three states now:

| answer | verdict | action |
|---|---|---|
| `ret:0` | Queued | tracked for confirmation |
| `ret` non-zero | Refused | really lost, said plainly |
| timeout, HTML, empty, JSON with no `ret` | Unresolved | retried |

Five attempts over 7.5 seconds. A refusal is **not** retried, because identical
bytes earn an identical answer. A refusal that arrives **after** an unresolved
attempt is reported as unresolved rather than as a loss: the likeliest reason a
node refuses a block it did not refuse a second ago is that it already holds it,
and crying loss there teaches an operator to distrust the line that is a real
loss.

## It stops moving money when it cannot write its own books

The settlement path always honoured a failed durable write. The block path threw
the answer away, and the block path is the one place where the write carries
something the pool cannot reconstruct: the hold-back that keeps the next
settlement from distributing a whole subsidy at zero confirmations.

Now a failed write on the block path sets `accounting_halt`, which no template
change can clear.

**The block is still submitted.** It is irreplaceable, the chain does not care
what the pool managed to write to disk, and refusing to submit would turn a
bookkeeping failure into a certain loss of the entire reward. What stops is the
movement of money.

### The limit of this, stated plainly

`accounting_halt` is not persisted. If the pool is restarted before the disk is
fixed, it reads a state file that never learned about the block, and it will
distribute that block's income at zero confirmations. A second write to the same
full disk would fail too, so this is not closed by a patch; it closes when the
ledger itself becomes durable. The operator message says so in as many words.

## What is NOT implemented

Named here so nobody reads the sections above as a complete safety story.

- **A tip freshness bound that cannot be fooled.** One node cannot tell "the chain is quiet" from "this node is stuck": both look like an old tip. The startup bound is 3600s and the running bound 7200s, measured against 200 real mainnet blocks whose largest gap was 2013s. A node stalled for less than an hour is accepted.
- **Peer count, best-known-peer height, sync-complete and validation mode.** The
  node's `/query/latest` returns only `height` and `diamond`. Real readiness
  needs a new node endpoint, which is a change to the node and not to the pool.
- **A pool state machine.** There are three halts and one accessor, which is the
  embryo of one, but there is no explicit STARTING / SYNCING / READY /
  DEGRADED / RECOVERY state exposed anywhere.
- **Frozen block entitlements, as a payment rule.** The pool now RECORDS who
  earned each block, at the instant it is found and in the same durable
  snapshot, and reports what the two models would pay when they differ. It still
  pays the old way: over whoever holds PPLNS credit at settlement time. Stage
  two, which makes the recorded answer the paying one, needs real blocks to
  compare first. See [ARCHITECTURE.md](ARCHITECTURE.md).
- **Authenticated jobs.** The miner can now reach a pool over https, which
  removes the position an attacker needs. The pool still credits a share to
  whatever payout address the query string names, so on plain HTTP anyone on the
  path can resend a miner's work under their own address.
- **Signer isolation.** The wallet key lives in the same process that serves the
  public listener.
