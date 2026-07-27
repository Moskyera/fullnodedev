# Running HBIT (`hbit-pool-server` + `hbit-pool-payout`)

HBIT is the mining pool this project builds, and this is the operator runbook
for it: the program that serves work to other people's miners, keeps PPLNS share
accounting, submits found blocks and pays everybody out. It handles **real money
that is not yours**, so read the warnings below before you start it. Each one
changes something an operator can see, and not knowing about it is how people
lose coins or think the pool is broken.

If you have not read `README-POOL.txt` yet, read that first. It is the short
version of why you would run a pool at all. This file is what you keep open
while you do.

In a release download both programs are already built and sit next to the miner.
From a source checkout they live in the `hbit-pool` crate and are built with
`cargo build --release -p hbit-pool`.

Design background: **[COMMUNITY-POOL-DESIGN.md](COMMUNITY-POOL-DESIGN.md)**.
The separate free-IP work relay (`hac-pool`) is **[PUBLIC-POOL.md](PUBLIC-POOL.md)**.

| Program | What it does |
|---------|--------------|
| `hbit-pool-server` | Serves work, validates shares, submits blocks, settles on a timer |
| `hbit-pool-payout` | Manual settlement, run by hand when the server is stopped |

```
hbit-pool-server <node> <wallet_file> <listen> <share_bits> <chain> [settle_secs]
hbit-pool-payout <pool_base> <node> <chain> [wallet_file] [reserve_units] [dust_units] [--commit]
```

**Both programs print all of that themselves.** `hbit-pool-server --help` and
`hbit-pool-payout --help` describe every argument, with a working example and
the two settings a miner needs; the `hbit-pool.example.ini` worksheet that ships
beside them is the same list with room to write your own answers in. You never
have to guess an argument from this file, and neither program ever asks you a
question: everything is an argument or an environment variable, so both run
unattended under a service manager.

---

## 0. The first ten minutes

**Before the first start.** Have your own Hacash fullnode running and synced.
The pool talks to its API, whose port is the `[server] listen` value in that
node's `hacash.config.ini`; the config this package ships uses **8080**, so the
node URL is normally `http://127.0.0.1:8080`. Set a wallet passphrase in the
same window you are about to start the pool in (section 1), because the wallet
is created on the first start and it is encrypted only if a passphrase is set
then.

**The first start.** Bind to loopback while you look around, so nobody can mine
here yet:

```powershell
$env:HBIT_WALLET_PASSWORD = "a long passphrase you have written down"
.\hbit-pool-server.exe http://127.0.0.1:8080 pool-wallet.key 127.0.0.1:9777 24 mainnet
```

```bash
export HBIT_WALLET_PASSWORD='a long passphrase you have written down'
./hbit-pool-server http://127.0.0.1:8080 pool-wallet.key 127.0.0.1:9777 24 mainnet
```

It refuses to start, with an explanation and a `What to do:` line, if the node
is not answering, if the chain argument does not match that node, if the listen
address is wrong or its port is taken, if `share_bits` or `settle_secs` is not a
number in range, or if another copy is already running on the same wallet.
Nothing is mined and nothing is paid when it refuses.

**What a good start looks like.** Just before `listening on` it prints a
readback. Check every line of it:

```
----------------------------------------------------------------------
 HBIT pool is up. Read this back before you let anyone mine here.
   pays FROM   <your pool's address>
               key file pool-wallet.key, ENCRYPTED at rest
   follows     http://127.0.0.1:8080 (mainnet, at block <height>)
   terms       PPLNS over the last 4096 shares, no pool fee, minimum payout 0.1 HAC
               block income payable 16 blocks after this pool finds it, settles every 5m
   share       2^24 easier to find than a network block
   miners set  connect = <the address and port miners use>
               pool_worker = <that miner's own HAC address>
               loopback only: no other machine can mine here. Bind 0.0.0.0 when you are ready
   check it    http://<the address and port miners use>/terms
----------------------------------------------------------------------
```

- **pays FROM** is the wallet every payout comes out of. It is the address you
  back up, and the one to check on a block explorer.
- **key file** says what is really on the disk. `PLAINTEXT on disk` means no
  passphrase was set: fix that before real money arrives (section 1).
- **follows** must be your own node, and the height must be the real chain tip.
- **terms** is read out of the same constants the payout code uses, so it is
  what your miners will actually get. It is the same thing `/terms` serves them.
- **miners set** is the line to paste to a miner. If the pool is bound to
  `0.0.0.0` it cannot know your public address, so it says so instead of
  inventing one.

**On the very first start only**, a second block follows it: the pool created
its wallet. Back that file up before you go any further; section 1 is the whole
of why.

**Then open it up.** Stop the pool, restart it with `0.0.0.0:9777` in place of
`127.0.0.1:9777`, and give a miner the `connect` and `pool_worker` lines above.
Check `http://<your pool>/terms` and `http://<your pool>/earnings?worker=<a
miner's address>` from another machine to confirm it is reachable.

---

## 1. The wallet file can now be encrypted, and the passphrase is half the key

The pool wallet key file (default `pool-wallet.key`) holds the private key that
controls **every coin the pool has taken in but not yet paid out**. It can now be
stored encrypted with Argon2id + AES-256-GCM.

Set a passphrase in one of these two environment variables before starting
`hbit-pool-server` or `hbit-pool-payout`:

| Variable | Meaning |
|----------|---------|
| `HBIT_WALLET_PASSWORD` | The passphrase itself |
| `HBIT_WALLET_PASSWORD_FILE` | Path to a file holding the passphrase (for services that cannot carry secrets in the environment) |

`HBIT_WALLET_PASSWORD` wins if both are set. The passphrase must be at least 8
characters; anything shorter is refused at startup rather than silently accepted.

Windows PowerShell:

```powershell
$env:HBIT_WALLET_PASSWORD = "a long passphrase you have written down"
.\hbit-pool-server.exe http://127.0.0.1:8080 pool-wallet.key 0.0.0.0:9777 24 mainnet
```

Linux:

```bash
export HBIT_WALLET_PASSWORD='a long passphrase you have written down'
./hbit-pool-server http://127.0.0.1:8080 pool-wallet.key 0.0.0.0:9777 24 mainnet
```

A passphrase shorter than 8 characters, or a `HBIT_WALLET_PASSWORD_FILE` that
cannot be read, stops the pool before it touches a key, with a message saying
which one it was. It never falls back to writing the key in the clear because
the passphrase was faulty.

What happens next:

- **No wallet file yet:** a new wallet is generated and written **encrypted**.
  The pool then prints a block you cannot miss, as the last thing before it
  starts serving: the file, the address, and what losing either half costs.
- **An existing plaintext wallet file:** it is **migrated automatically** the
  next time the wallet is loaded. The encrypted form is decrypted and compared
  against the original key before it replaces the file, so a failed migration can
  never cost you the wallet. The pool prints `[wallet] <file> is now ENCRYPTED`.
- **No passphrase set:** the file stays plaintext and the pool prints a loud
  warning every time it starts. This still works, it is just not protected.

Either way the startup readback states which it is, every single start:
`key file pool-wallet.key, ENCRYPTED at rest` or `key file pool-wallet.key,
PLAINTEXT on disk, no passphrase set`. That line is read from the file itself,
not from the environment, so it says what is really on the disk.

### Back up the passphrase ALONGSIDE the key file

This is the part that loses money if you skip it.

- **The key file alone is useless without the passphrase.** There is no reset, no
  recovery question and no support address. If you back up the encrypted file and
  forget the passphrase, every coin the pool holds is gone for good.
- **The passphrase alone is useless without the key file.** Both halves must
  survive whatever kills the machine. Keep them together in the same safe place,
  or keep both in two separate safe places. Do not put one on the mining rig and
  the other nowhere.
- Test the pair before you trust it: with the pool stopped, restore the backed-up
  file to a scratch directory, set the passphrase and start `hbit-pool-payout` in
  its default dry-run mode. It prints the wallet address. If that address matches
  your live pool address, the backup works.

### Your OLD plaintext copies are still out there

Encrypting the file today does **not** reach backwards. The plaintext private key
may still exist in:

- ordinary file backups taken before the migration,
- Windows shadow copies / VSS snapshots and Linux filesystem or VM snapshots,
- cloud sync folders and their version history,
- old drives, images and machines you no longer use.

Anything holding one of those copies can spend the pool's funds, passphrase or
not. Treat every pre-migration backup and snapshot as a live secret: destroy the
ones you do not need, and keep the ones you do need under the same protection you
would give cash.

If you believe a plaintext copy leaked, the only real fix is a new wallet: stop
the pool, run `hbit-pool-payout --commit` to pay everyone out of the old wallet,
move the remainder to your own address, then start the pool with a fresh wallet
file and a fresh passphrase.

---

## 2. `hbit-pool-payout` will not run while `hbit-pool-server` is running

`hbit-pool-server` now takes an **exclusive OS lock on the wallet** for its whole
run, and `hbit-pool-payout` takes the same lock. So:

- `hbit-pool-payout` started while the server is up **refuses to run and exits
  non-zero**, printing `REFUSING to run: another hbit-pool-server or
  hbit-pool-payout already holds <wallet file>`.
- `hbit-pool-server` started while `hbit-pool-payout` is mid-run refuses to start
  the same way.

**This is deliberate and it is protecting your money.** Both programs decide what
to pay from the wallet's *confirmed* balance, and a payout sitting in the mempool
does not reduce that balance. Run them at once and each one sees the full balance,
each one believes it is the only settler, and the same PPLNS window gets paid
**twice** out of the operator's own funds. The lock is what makes that impossible.

The lock is held by the operating system, so a crash or a kill releases it
immediately. There is nothing to clean up by hand, and **deleting the
`<wallet_file>.settle.lock` file does not release anything**: the lock belongs to
the running process, not to the file, so removing it would only let two payers
run at once. Both refusals say so themselves.

### Correct procedure for a manual payout

1. **Stop `hbit-pool-server`** and wait for the process to actually exit.
2. Run the tool in its **dry-run** default first and read the planned split:
   ```bash
   ./hbit-pool-payout http://127.0.0.1:9777 http://127.0.0.1:8080 mainnet pool-wallet.key
   ```
   It pays nothing without `--commit`.
3. If the split looks right, run it again with `--commit`.
4. **Restart `hbit-pool-server`.**

Set `HBIT_WALLET_PASSWORD` in that window too if the wallet is encrypted, and
run it **in the folder that holds the wallet file** (or pass the path as
argument 4). The tool never creates a wallet: if there is no key file where it
was told to look, it refuses and says so, rather than making a fresh empty one
and reporting that you have nothing to pay.

If the node is not answering it refuses as well, instead of reading an empty
balance as a zero balance.

While the server is stopped its `/stats` endpoint cannot answer, so
`hbit-pool-payout` reads the share window out of the accounting file the server
left next to the wallet (`<wallet_file>.state.json`). Keep that file with the
wallet file; it also carries the shared pending-payout ledger that stops a
re-run, a crash or an overlapping cron job paying the same window twice.

---

## 3. Payouts lag block discovery by about 16 blocks

When the pool finds a block, that block's coinbase reward is **held back from
settlement until the chain has buried the block 16 blocks deep**. Only then does
it join the distributable balance.

On mainnet a block is targeted at 5 minutes, so **income from a block you just
found becomes payable roughly 80 minutes later**. While it waits, the pool prints

```
[settle] holding back N unit(s) of block income that is not yet buried 16 deep;
nothing matured to pay this cycle
```

**Nothing is stuck and nothing is missing.** The reason for the delay:

- A freshly found block can still be **orphaned** by a reorg. The node itself
  treats the last 4 blocks as reorg-able.
- If the pool paid that block's reward out immediately and the block were then
  orphaned, the income would vanish from the canonical chain while the payout
  transaction that spent it stays perfectly valid. The miners keep the coins, the
  chain never delivers the reward, and **the operator eats the whole subsidy out
  of their own pocket** with no way to recover it.
- 16 confirmations puts a wide margin over the node's own reorg window, so this
  can only happen after a reorg deeper than anything the network has ever seen.

An orphan is detected and the held-back amount is simply dropped, never paid.
Confirmed blocks and orphans are both counted on `/stats`.

Practical consequences to tell your miners about:

- The first payout after the pool's very first block arrives roughly 80 minutes
  after that block, not immediately.
- Steady state is unaffected: once the pool is finding blocks regularly, the
  16-block lag is a constant offset, not a growing backlog.
- Payouts below the dust floor (default 0.1 HAC) roll over to the next window
  instead of being paid, which is a separate and expected reason a small miner
  sees nothing on a given cycle.

---

## 4. `hbit-pool-server` refuses to start on a bad configuration

The server checks its own configuration before it serves a single piece of work.
Each check below exits with status 2 and an explanation ending in a `What to do:`
line, instead of running in a state that would quietly lose money. **When it
refuses, nothing has been mined and nothing has been paid.** On the checks that
run before the wallet is opened it does not even create a wallet file, so a
mistyped first attempt leaves nothing behind to tidy up.

### Every argument is required, and none is guessed

All five positional arguments must be present. A number that is not a number is
**refused, never replaced by the default**: an operator who mistyped `share_bits`
would otherwise mine for weeks on a share size they did not choose and were never
told about. Running the program with no arguments, or with `--help`, prints the
whole usage text.

### `share_bits` must be between 18 and 40

`share_bits` (argument 4) says how many powers of two easier a share is than a
real network block. Outside `18..=40` the server prints
`<share_bits> must be between 18 and 40 (got N)` and exits.

- **Below 18:** shares get so hard that the 4096-share PPLNS window covers a
  meaningful slice of a block interval. A difficulty change landing inside a live
  window then splits real payout money by share counts that stand for different
  amounts of work.
- **Above 40:** shares get so easy that a whole GPU batch always beats one, so
  credit tracks batch cadence rather than hashrate, and the share target
  degenerates.

**20 is the recommended value**, and the shipped `deploy/docker-compose.yml`
uses it. Measured on the live chain on 2026-07-27, a block costs 2^42, so 20
leaves each share costing 2^22 hashes. That choice is about payout memory as much
as about size: PPLNS pays on the last 4096 shares, so EASIER shares make that
window cover LESS TIME. At `share_bits` 24 an ordinary card produces roughly 26
shares a second, and a pool with ten miners turns its whole window over in about
fifteen seconds, so a miner that drops off for half a minute loses everything it
was owed. At 20 the same pool keeps about four minutes of history.

### The live difficulty is checked too, and it is checked twice

The range above is only about the number you typed. Once the pool has fetched a
template it checks the target it would really serve, and there are TWO ways that
can fail, because a ratio and a cost are different questions.

- **The ratio.** `share_target = network_target * 2^share_bits` saturates at the
  all-0xff ceiling, so on a very easy chain what workers get is not what was
  asked for. If the achieved factor falls below 18 the pool refuses.
- **The cost.** A share must be worth at least 2^16 hashes. This is the bound
  that matters and it is NOT implied by the first one: a chain whose target has
  22 leading zero bits, served with `share_bits` 24, reports an achieved factor
  of 22, clears the ratio bound comfortably, and still hands out a share target
  that every hash on earth beats.

In both cases the reason is the same. When a share costs nothing, PPLNS credit
measures how fast a worker completes an HTTP round trip instead of how much it
hashed, so the fastest submitter takes the window from miners doing more work.
The pool will not distribute real money on that basis.

The remedies are opposite, so read which one you got. Since
`leading_zero_bits(network) = achieved + cost`, lowering `share_bits` moves work
out of the ratio and into the cost, and the message names the highest value that
would work. Only when the chain cannot support any legal setting, which is where
a fresh testnet always sits, does the pool tell you that nothing here helps and
that the ceiling is the chain's rather than yours.

### `settle_secs` must be between 30 and 86400

`settle_secs` (argument 6, default 300) is the automatic payout interval. Each
settlement is a signed transaction that carries a network fee, so paying out
every few seconds spends the reserve for nothing, and `0` would leave the
settlement thread spinning against the node with no pause at all. Leave it out
unless you have a reason.

### The node has to be there, and it has to be yours

Before anything else the pool asks the node for its current block. If nothing
answers it refuses, naming the URL it tried and where the port comes from (the
`[server] listen` value in the node's `hacash.config.ini`, 8080 in the config
this package ships). A node that is still syncing, or that would not hand over a
block template, is refused the same way.

### The listen address has to be usable

The pool binds its port **before** it opens the wallet, so the two commonest
first-run mistakes here cost nothing: a `<listen>` with no port in it, and a port
something else already holds. Both are refused with the correct form and the
likely cause.

### The test routes require `worker=<HAC address>`

The `/work` and `/share` test routes now demand a real, payable HAC address:

```
/work?worker=<your HAC address>
/share?worker=<your HAC address>&height=...&nonce=...
```

The old placeholder worker name `w1` (and any other non-address name) is
**rejected** with `set worker=<your HAC address> so the pool can pay you`. The
standard `/submit/miner/success` route enforces the same rule via `pool_worker`.

This is not pedantry. Share credit is keyed by payout address, and the PPLNS
window is a fixed 4096 shares shared by everybody. A share credited to a name the
pool cannot pay is work done for nothing that **also evicts a payable miner's
share** from the window, so small and intermittent miners drop out of the window
before a block is found. Any script or monitoring check still using `worker=w1`
must be updated.

### The `chain` argument is required and is proved against the node

`chain` (argument 5) has no default, because a pool running the wrong difficulty
rule mines work the node rejects forever without saying so. Accepted values:

| Value | Use for |
|-------|---------|
| `mainnet` | Real Hacash mainnet (consensus-fixed 288 blocks / 300s) |
| `testnet` | A testnet running the documented 288 / 10s pair |
| `testnet:<difficulty_adjust_blocks>:<each_block_target_time>` | A testnet configured with any other pair |

The third form exists because a testnet node reads `difficulty_adjust_blocks` and
`each_block_target_time` from its **own** `hacash.config.ini`, so the label alone
proves nothing. Spell out the pair your node actually uses.

At startup the server **recomputes the difficulty of the node's own current tip**
and compares it with what the node stored. If they do not match it prints
`REFUSING to start: difficulty rule mismatch at the node's own tip ...` and exits.
An exact match is the only proof that the rule in force here is the one the node
validates with. If you see this error, fix the chain argument; do not work around
it, because every block the pool finds would otherwise be thrown away.

`hbit-pool-payout` takes the same required `chain` argument in the same three
forms.

---

## 5. Telling miners how to reach your pool

The miner panel lists **HBIT pool** first in its pool picker, and it ships that
entry with **no address**, because no HBIT address is published in this
repository and an invented one is an address somebody would paste in and point
real hashrate at. You publish yours.

A miner that is configuring `poworker` by hand needs exactly two settings, and
the pool prints them for you in its startup readback:

```
connect = <your pool's address and port>
pool_worker = <that miner's own HAC address>
```

`pool_worker` is where that miner gets paid, so it is theirs, not yours. If your
pool is bound to `0.0.0.0` it cannot know what address the outside world reaches
it on, so the readback says `<this machine's IP address or hostname>` instead of
inventing one: that part is for you to fill in.

For the panel, drop a `pools.json` next to `miner-panel.exe` on the miner's PC:

```json
[
  {"name": "HBIT pool", "connect": "pool.example:9777"}
]
```

The name is matched case insensitively against the built-in entry, so this
replaces the HBIT entry in place rather than adding a second one; it keeps its
first position, and the panel reads the file when the miner presses Refresh next
to the picker, with no rebuild. Two things to know:

- Overriding an entry replaces the whole entry, so the built-in note disappears
  and the panel falls back to its generic pool hint. Set `"note"` yourself if
  you want one. A note that promises a payout scheme, a fee level or a minimum
  is refused and replaced by the panel: those are claims it cannot check.
- `"verified": true` is the panel's own statement that it reached the endpoint.
  Leave it out.

What the panel shows about your terms does not come from that file. It comes
from your running pool: the dashboard reads `/terms` and `/earnings` from
`hbit-pool-server` and shows the miner your real scheme, fee and minimum payout,
and what you have already paid that miner. That is the whole reason HBIT can be
listed honestly next to pools the panel has never spoken to.

---

## Quick reference

Every refusal below ends with its own `What to do:` line; this table is for
finding the one you are looking at.

| Symptom | Cause | Fix |
|---------|-------|-----|
| `REFUSING to run: another hbit-pool-server or hbit-pool-payout already holds ...` | Both settlers running at once | Stop `hbit-pool-server`, run the tool, restart the server. Do not delete the `.settle.lock` file: it frees nothing |
| `REFUSING to start: no Hacash fullnode answered at ...` | Node not running, still starting, or wrong URL/port | Start and sync the node; use the `[server] listen` port from its `hacash.config.ini` (8080 here) |
| `REFUSING to start: the node ... would not give a block template` | Node is up but not ready to be mined on | Let it finish syncing, then start the pool again |
| `REFUSING to start: cannot listen on ...` | `<listen>` is not `<ip>:<port>`, or the port is taken | Use `0.0.0.0:9777` or `127.0.0.1:9777`; if the form is right, something else holds that port |
| `wallet file ... is encrypted but no passphrase is configured` | Passphrase missing from the environment | Set `HBIT_WALLET_PASSWORD` or `HBIT_WALLET_PASSWORD_FILE` |
| `cannot decrypt wallet file ...` | Wrong passphrase, or a corrupted file | Use the backed-up passphrase; restore the file from backup |
| `the wallet passphrase in ... must be at least 8` | Passphrase too short to protect real money | Set a longer one, written down somewhere physical |
| `<share_bits> must be between 18 and 40` | Out-of-range or mistyped argument 4 | Use 20 |
| `the network difficulty in force ... is too low to serve a fair share` | The chain is so easy the share target saturates, so the achieved ratio is under 18 | Point the pool at mainnet. Lowering `share_bits` cannot help here |
| `the network difficulty in force ... is too low to serve a share worth counting` | The ratio is fine but a share would cost under 2^16 hashes | Lower `share_bits` to the value the message names; if it says nothing helps, the chain itself is too easy |
| `[settle_secs] must be between 30 and 86400` | Out-of-range or mistyped argument 6 | Leave it out to get 300 |
| `set worker=<your HAC address> so the pool can pay you` | Worker name is not a payable address | Pass the miner's real HAC address |
| `REFUSING to start: difficulty rule mismatch ...` | Wrong `chain` argument for this node | Use `mainnet`, or spell out `testnet:<adjust_blocks>:<target_time>` |
| `REFUSING to run: there is no wallet file at ...` | `hbit-pool-payout` run in the wrong folder | Run it where the wallet file is, or pass its path as argument 4 |
| `[settle] holding back N unit(s) ...` | Recently found block not yet 16 deep | Nothing to do, wait about 80 minutes |
| Miners cannot connect at all | Pool bound to `127.0.0.1`, so only this machine can reach it | Restart with `0.0.0.0:<port>`; the readback says which one it is |
