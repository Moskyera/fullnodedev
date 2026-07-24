# Running the pool (`pool-server` + `pool-payout`)

This is the operator runbook for the payout pool: the program that serves work to
other people's miners, keeps PPLNS share accounting, submits found blocks and
pays everybody out. It handles **real money that is not yours**, so read the four
warnings below before you start it. Each one changes something an operator can
see, and not knowing about it is how people lose coins or think the pool is
broken.

Design background: **[COMMUNITY-POOL-DESIGN.md](COMMUNITY-POOL-DESIGN.md)**.
The separate free-IP work relay (`hac-pool`) is **[PUBLIC-POOL.md](PUBLIC-POOL.md)**.

| Program | What it does |
|---------|--------------|
| `pool-server` | Serves work, validates shares, submits blocks, settles on a timer |
| `pool-payout` | Manual settlement, run by hand when the server is stopped |

```
pool-server <node> <wallet_file> <listen> <share_bits> <chain> [settle_secs]
pool-payout <pool_base> <node> <chain> [wallet_file] [reserve_units] [dust_units] [--commit]
```

---

## 1. The wallet file can now be encrypted, and the passphrase is half the key

The pool wallet key file (default `pool-wallet.key`) holds the private key that
controls **every coin the pool has taken in but not yet paid out**. It can now be
stored encrypted with Argon2id + AES-256-GCM.

Set a passphrase in one of these two environment variables before starting
`pool-server` or `pool-payout`:

| Variable | Meaning |
|----------|---------|
| `HBIT_WALLET_PASSWORD` | The passphrase itself |
| `HBIT_WALLET_PASSWORD_FILE` | Path to a file holding the passphrase (for services that cannot carry secrets in the environment) |

`HBIT_WALLET_PASSWORD` wins if both are set. The passphrase must be at least 8
characters; anything shorter is refused at startup rather than silently accepted.

Windows PowerShell:

```powershell
$env:HBIT_WALLET_PASSWORD = "a long passphrase you have written down"
.\pool-server.exe http://127.0.0.1:8088 pool-wallet.key 0.0.0.0:9777 24 mainnet
```

Linux:

```bash
export HBIT_WALLET_PASSWORD='a long passphrase you have written down'
./pool-server http://127.0.0.1:8088 pool-wallet.key 0.0.0.0:9777 24 mainnet
```

What happens next:

- **No wallet file yet:** a new wallet is generated and written **encrypted**.
- **An existing plaintext wallet file:** it is **migrated automatically** the
  next time the wallet is loaded. The encrypted form is decrypted and compared
  against the original key before it replaces the file, so a failed migration can
  never cost you the wallet. The pool prints `[wallet] <file> is now ENCRYPTED`.
- **No passphrase set:** the file stays plaintext and the pool prints a loud
  warning every time it starts. This still works, it is just not protected.

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
  file to a scratch directory, set the passphrase and start `pool-payout` in its
  default dry-run mode. It prints the wallet address. If that address matches your
  live pool address, the backup works.

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
the pool, run `pool-payout --commit` to pay everyone out of the old wallet, move
the remainder to your own address, then start the pool with a fresh wallet file
and a fresh passphrase.

---

## 2. `pool-payout` will not run while `pool-server` is running

`pool-server` now takes an **exclusive OS lock on the wallet** for its whole run,
and `pool-payout` takes the same lock. So:

- `pool-payout` started while the server is up **refuses to run and exits
  non-zero**, printing `REFUSING to run: another pool-server or pool-payout
  already holds <wallet file>`.
- `pool-server` started while `pool-payout` is mid-run refuses to start the same
  way.

**This is deliberate and it is protecting your money.** Both programs decide what
to pay from the wallet's *confirmed* balance, and a payout sitting in the mempool
does not reduce that balance. Run them at once and each one sees the full balance,
each one believes it is the only settler, and the same PPLNS window gets paid
**twice** out of the operator's own funds. The lock is what makes that impossible.

The lock is held by the operating system, so a crash or a kill releases it
immediately. There is no stale lock file to clean up by hand.

### Correct procedure for a manual payout

1. **Stop `pool-server`** and wait for the process to actually exit.
2. Run the tool in its **dry-run** default first and read the planned split:
   ```bash
   ./pool-payout http://127.0.0.1:9777 http://127.0.0.1:8088 mainnet pool-wallet.key
   ```
   It pays nothing without `--commit`.
3. If the split looks right, run it again with `--commit`.
4. **Restart `pool-server`.**

While the server is stopped its `/stats` endpoint cannot answer, so `pool-payout`
reads the share window out of the accounting file the server left next to the
wallet (`<wallet_file>.state.json`). Keep that file with the wallet file; it also
carries the shared pending-payout ledger that stops a re-run, a crash or an
overlapping cron job paying the same window twice.

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

## 4. `pool-server` now refuses to start on a bad configuration

The server checks its own configuration before it serves a single piece of work.
Each check below exits with status 2 and an explanation instead of running in a
state that would quietly lose money.

### `share_bits` must be between 18 and 40

`share_bits` (argument 4, default 24) says how many powers of two easier a share
is than a real network block. Outside `18..=40` the server prints
`share_bits must be between 18 and 40 (got N)` and exits.

- **Below 18:** shares get so hard that the 4096-share PPLNS window covers a
  meaningful slice of a block interval. A difficulty change landing inside a live
  window then splits real payout money by share counts that stand for different
  amounts of work.
- **Above 40:** shares get so easy that a whole GPU batch always beats one, so
  credit tracks batch cadence rather than hashrate, and the share target
  degenerates.

24 suits GPU batches and is the right answer unless you have measured otherwise.

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

`pool-payout` takes the same required `chain` argument in the same three forms.

---

## Quick reference

| Symptom | Cause | Fix |
|---------|-------|-----|
| `REFUSING to run: another pool-server or pool-payout already holds ...` | Both settlers running at once | Stop `pool-server`, run the tool, restart the server |
| `wallet file ... is encrypted but no passphrase is configured` | Passphrase missing from the environment | Set `HBIT_WALLET_PASSWORD` or `HBIT_WALLET_PASSWORD_FILE` |
| `cannot decrypt wallet file ...` | Wrong passphrase, or a corrupted file | Use the backed-up passphrase; restore the file from backup |
| `share_bits must be between 18 and 40` | Out-of-range argument 4 | Use 24 |
| `set worker=<your HAC address> so the pool can pay you` | Worker name is not a payable address | Pass the miner's real HAC address |
| `REFUSING to start: difficulty rule mismatch ...` | Wrong `chain` argument for this node | Use `mainnet`, or spell out `testnet:<adjust_blocks>:<target_time>` |
| `[settle] holding back N unit(s) ...` | Recently found block not yet 16 deep | Nothing to do, wait about 80 minutes |
