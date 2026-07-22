# Hacash Community Pool — Design

A trust-minimized mining pool for newcomers, designed within what the Hacash
mainnet actually allows today (verified against the node source, 2026-07).
Goal: small GPUs (RTX 3050, RX 9060 XT) get **smooth, frequent, fair** payouts —
without the operator taking meaningful custody of anyone's funds.

This document is the plan. It is intentionally phased so we ship value early and
add trust-minimization on top, rather than building everything before anything
works.

---

## 1. Philosophy

- **Newcomer-first.** A user picks the pool in the panel, pastes their HAC
  address, presses Start. No CLI, no config files.
- **Honest.** We never call something a "payout pool" until it pays fairly, and
  we always disclose the custody model in plain language in the UI.
- **Trust-minimized, not custodial-forever.** Custody is bounded, guarded, and
  ultimately escapable by miners (see §6). We never hold more than a short
  settlement window's worth, and never behind a single key.
- **No consensus fork.** Everything here runs on the node **as-is**. No changes
  to Hacash consensus; the pool is off-node software plus normal transactions.

---

## 2. The constraints we must design within (verified facts)

These are the "physics". Every design choice below follows from them.

| Fact | Consequence | Source |
|------|-------------|--------|
| Coinbase is **single-output**: one PRIVAKEY address, `reward == block_reward(height)` | A block reward cannot be split on-chain among many miners | `mint/src/check/coinbase.rs:12-18,114-142` |
| Consensus does **not** bind the coinbase to node config — `/submit/block` accepts any valid block with any PRIVAKEY coinbase | The pool can assemble blocks off-node and choose the coinbase address | `mint/src/api/submit_block.rs`; `mint/src/check/coinbase.rs:114-142` |
| Stock template API (`/query/miner/pending`) hardwires coinbase to `[miner] reward`; worker submits only 2 nonces | To choose coinbase we must build templates ourselves, not ask the node | `mint/src/check/block_build.rs:27-31`; `mint/src/api/miner_success.rs` |
| **No "share" concept** anywhere — PoW is validated only against the full network target | Share accounting is 100% off-node (pool ↔ worker) | `mint/src/api/miner_success.rs:50`; `mint/src/check/block_accept.rs:28` |
| A normal transfer can be **any fractional amount**, and one tx carries up to **200 actions** (`TX_ACTIONS_MAX=200`) | The pool can pay ~200 miners fractional amounts in one cheap tx | `basis/src/component/action.rs` (TX_ACTIONS_MAX); `protocol/src/action/hacash.rs:4,55` (HacToTrs 1, HacFromToTrs 14) |
| "Istanbul" (mainnet activation at height **765432**, already live) unlocked: **type3 multisig** (≤200 signers), **VM contracts** (40/41/44), **P2SH scriptmh** (P2SHScriptProve 46) + VM native hashes + `ViewCheckSign` + `HeightScope`/`BalanceFloor` guards | We have a real toolkit to harden custody | `protocol/src/upgrade.rs:8,33-42,88-141`; `vm/src/action/p2sh.rs:86`; `vm/src/native/hash.rs` |
| Payment channels ship **cooperative open/close only**; trustless unilateral exit is modeled but **unregistered** | True off-chain streaming needs a node change — out of scope for v1/v2 | `mint/src/action/mod.rs:30-36`; `field/src/component/channel.rs` |
| Economics: block reward **8 HAC**, block time **5 min**, network **~26 GH/s** | A 10-GPU pool finds ~1 block / 3 h; a 3050 alone would wait ~11 days per block | `mint/src/genesis/reward.rs`; `mint/src/config.rs:9`; explorer.hacash.org |

**The core insight:** the "minimum payout = one whole block" barrier applies only
to the **coinbase**. If the pool receives whole blocks and **redistributes via
normal transfers**, it can pay each miner their exact fractional share, often and
cheaply. That is the whole design. The cost is custody between settlements, which
§6 bounds and hardens.

---

## 3. Architecture

```
   miners (poworker, modified)                 pool operator (e.g. home M6 + fullnode)
   ┌───────────────────────┐    work + shares  ┌──────────────────────────────────┐
   │  GPU/CPU, own wallet   │ ───────────────▶ │  hac-pool daemon                 │
   │  mines pool templates  │ ◀─────────────── │   • share validator (share target)│
   │  submits SHARES        │   share target   │   • share accounting (PPLNS)      │
   └───────────────────────┘                   │   • block assembler (own coinbase)│
                                                │   • settlement engine (batched)   │
   full-solution ─────────────────────────────▶│   • treasury (multisig / P2SH-HTLC)│
                                                └───────────────┬──────────────────┘
                                                                │ /submit/block, batched transfers
                                                                ▼
                                                        Hacash fullnode (unchanged)
```

Components (all off-node):

1. **Share validator.** Advertises a pool-chosen **share target** below the
   network target; validates each submitted share by re-running
   `x16rs::block_hash` over the reconstructed 89-byte block header and comparing
   to the share target (`hash_bigger_than`). Reusable primitives already exist.
2. **Share accounting.** PPLNS-style: keep a sliding window of the last *N*
   shares; each miner's credit = their share count in the window. Transparent
   (published log).
3. **Block assembler.** Re-implements the ~85 lines of `impl_packing_next_block`
   off-node: pulls `prevhash/height/difficulty/timestamp` from `/query/block/intro`
   + `/query/latest`, calls `create_coinbase_tx(height, msg, POOL_ADDRESS)`
   (already address-parameterized), builds `BlockV1`, computes the merkle root,
   exposes the header nonce slot. Coinbase pays the **pool treasury** (see §6).
4. **Settlement engine.** On a cadence (e.g. every *S* blocks), computes each
   miner's owed HAC from the PPLNS window and pays up to 200 miners in one
   batched transaction (`HacToTrs`), directly to their own wallets.
5. **Treasury.** Where block rewards land before settlement. Hardened per §6.

---

## 4. Share accounting model (PPLNS)

- Pool sets `share_target = network_target × D` where `D` (e.g. 1/1024) makes
  shares common enough for smooth accounting but not spammy.
- Each valid share credits the submitting miner 1 unit in a rolling window of the
  last *N* shares (window ≈ a few multiples of "shares per found block").
- When the pool finds a **full-network solution** (a share that also beats the
  real target), it submits the block via `/submit/block`; the 8 HAC lands in the
  treasury.
- A miner's entitlement over any period = (their shares in window) / (total shares
  in window) × (HAC the pool earned in that period). This is **PPLNS**: fair,
  hop-resistant, and standard.
- Everything is published (share log + per-miner running balance + settlement
  tx hashes) so anyone can verify payouts match work.

---

## 5. Settlement model

- **Batched fractional transfers.** Every *S* blocks (tunable; e.g. 12 blocks ≈
  1 hour), pay every miner whose accrued balance ≥ `min_payout` (dust floor,
  e.g. 0.01 HAC) in **one** `HacToTrs` tx carrying up to 200 outputs.
- **Sub-block granularity achieved.** A 3050 (~8 MH/s) accrues ~its fair fraction
  of every 8 HAC the pool earns, and gets it every settlement window — hours, not
  ~11 days. This is the entire point.
- **Fees.** Hacash fees are negligible today; one batched tx per window per ~200
  miners is cheap. Fee is paid by the treasury (a tiny pool fee %, disclosed).
- **Carry-over.** Balances below `min_payout` roll to the next window.

---

## 6. Trust / custody — bounded, guarded, escapable

This is a **custodial** design between settlements (the pool holds rewards before
redistributing) — there is no fully-trustless smooth option on Hacash today. We
minimize the trust in three stacked layers, shipped in order:

- **Layer A — Bound it (Phase 1).** Settle frequently (small *S*). The treasury
  never holds more than ~*S* blocks' worth. Publish everything. Trust = "operator
  won't run off with < one hour of pooled reward, in public."
- **Layer B — Guard it (Phase 2).** Make the treasury a **type3 multisig**
  (`ReqSignList`, ≤200 signers). Funds move only with M-of-N community signatures,
  so no single operator key can move pooled funds.
  Evidence: `protocol/src/transaction/type3.rs`, `protocol/src/action/reqsign.rs`.
- **Layer C — Make it escapable (Phase 3).** Escrow each miner's accrued balance
  in a **P2SH lockbox** (`P2SHScriptProve` 46) that the miner can **self-claim**
  (hashlock / their key) with a **height-locked refund** — an HTLC-shaped escrow.
  If the operator disappears, miners claim their owed balance themselves. This is
  the closest thing to trustless the chain offers without a node change.
  Evidence: `vm/src/action/p2sh.rs:86`; `vm/src/native/hash.rs`; `HeightScope`
  `protocol/src/action/chain.rs:33`; `ViewCheckSign` `vm/src/action/envfunc.rs:82`.

What we explicitly do **not** do:
- No multi-output/split coinbase — needs a consensus change (org-level).
- No payment-channel streaming — needs registering the modeled channel challenge
  action (a node/consensus change).
- No opaque custody. If we can't disclose it and bound it, we don't ship it.

---

## 7. Worker changes (poworker — we own it)

Today `poworker` talks only to the node's fixed-template API and submits full
solutions (`app/src/poworker.rs`). For the pool it must, additionally:

1. Pull the pool's template (coinbase = pool treasury) instead of the node's.
2. Mine against the pool's **share target** and submit **shares** (partial proofs)
   to the pool over a small pool↔worker protocol (not the node's 2-nonce submit).
3. Report its **payout address** to the pool once at connect.

This is client software we control; no node change. The GUI adds a "Pool" mode
that already exists — it just points at the pool endpoint (see the pool directory
work in `miner-panel/src/connect.rs`).

---

## 8. Phased roadmap

| Phase | Deliverable | Custody | Effort |
|-------|-------------|---------|--------|
| **P0** | Honest work-relay / shared node (done) | none | shipped |
| **P1** | Batched PPLNS pool: share validator + accounting + block assembler + batched settlement + modified worker. Frequent, fair, fractional payouts. Layer A trust. | bounded (~1 window) | the real build |
| **P2** | Multisig treasury (Layer B) | bounded + no single key | small, on top of P1 |
| **P3** | P2SH-HTLC per-miner escrow (Layer C) | escapable by miners | larger (lockbox bytecode + audit) |

Ship P1, run it on the M6 with a few friends, prove demand, then P2/P3.

---

## 9. Parameters (initial guesses — tune on testnet first)

- Share difficulty factor `D`: start 1/1024, adjust so a mid-GPU emits a few
  shares/minute.
- PPLNS window `N`: ≈ 3× (expected shares per found block).
- Settlement cadence `S`: 12 blocks (~1 h) for P1; shorten if treasury feels big.
- `min_payout` dust floor: 0.01 HAC. Pool fee: small, disclosed (e.g. 1%).

---

## 10. Risks & things to verify at implementation

- **Off-node assembly must be byte-exact.** Serialization, merkle prelude,
  `x16rs::block_hash`, and next-difficulty (ASERT) must match the node or blocks
  are rejected. Mitigation: the pool is a Rust process **linking the workspace
  crates** (`mint`/`protocol`/`x16rs`/`basis`) — reuse, don't reimplement. Verify
  exact struct field names/serialization against `protocol/src/block/` and
  `mint/src/check/block_build.rs` before coding.
- **Testnet first.** Every gate is bypassed on non-zero `chain_id`
  (`protocol/src/upgrade.rs:11`) — build and test the whole flow on testnet with
  fake money before a single HAC of real reward is at stake.
- **Fees / mempool.** No mempool-dump RPC exists, so off-node templates are
  coinbase-only (no fee txs). Fine while fees ≈ 0; revisit if that changes.
- **Reachability.** A home pool needs a public reachable IP (NAT/CGNAT blocks it)
  — orthogonal to this design but required for others to join (see the panel's
  reachability caveat).
- **Reorgs / stale.** Standard pool concerns: handle share timing across block
  changes, don't credit shares for a stale height.

---

## 11. Bottom line

On Hacash today the best realizable pool for newcomers is a **transparent PPLNS
pool with frequent batched fractional settlement**, hardened with multisig and
optional P2SH-HTLC escrow. It gives small GPUs smooth, fair, frequent payouts —
the thing whole-block rotation cannot — while keeping custody bounded, guarded,
and (in P3) escapable. Fully trustless smooth payouts would require a consensus
change (multi-output coinbase) or activating trustless channels; both are
node/org-level and out of scope for a community build.
