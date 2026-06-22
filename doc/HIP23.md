# HIP-23: Istanbul DeFi Application Patterns

**Status:** Draft (branch `hip-23-draft`)  
**Type:** Application / integration standard — **no consensus fork**  
**Activation:** Istanbul capabilities @ mainnet height `765432` (`ONLINE_OPEN_HEIGHT`)  
**Depends on:** Type3 transactions, ActionGuard, TEX, AST, HIP20 (`AssetCreate`), HVM (optional)

## 1. Purpose

HIP-23 documents **reusable DeFi composition patterns** on post-Istanbul Hacash. It does not introduce new protocol actions, fork heights, or consensus rules. Wallets, indexers, gateways, and integrators MAY implement these patterns today using existing Istanbul machinery.

Goals:

- Give builders copy-paste-safe transaction templates for common financial flows.
- Tie each pattern to **normative MUST/SHOULD** rules wallets and indexers can validate off-chain.
- Provide regression tests (`tests/hip23_pattern_regression.rs`) that lock pattern semantics to `fullnodedev`.

Reference material:

- Istanbul capability model: `istanbul_upgrade_tech.md` (community tech note)
- TEX settlement: `protocol/src/tex/*`, `tests/tex.rs` (`trs1`)
- AST control flow: `doc/ast-spec.md`
- Guards: `protocol/src/action/chain.rs`

## 2. Scope

### 2.1 In scope (v1)

| ID | Pattern | Primary primitives |
|----|---------|-------------------|
| P1 | Atomic multi-asset TEX swap | `TexCellAct` (kind 22), transfer cells 1–8 |
| P2 | Time-boxed guarded payment | `HeightScope` (0x0412) + top transfer |
| P3 | BalanceFloor protected transfer | `BalanceFloor` (0x0413) + ordered debits |
| P4 | HIP20 issuance + TEX distribution | `AssetCreate` (16) + asset TEX cells 7/8 |
| P5 | AST conditional settlement | `AstIf` / `AstSelect` + `HeightScope` condition |

### 2.2 Out of scope (v1)

- New action kinds or fork gates
- HVM contract templates (future HIP-23 v2 MAY add optional contract companions)
- Cross-chain bridges, oracle networks, or mempool policy
- Fee-market or MEV recommendations

## 3. Common requirements

All patterns MUST satisfy:

1. **Transaction type:** `type >= 3` when AST, TEX asset cells, or gas metering are used.
2. **Height gate:** On mainnet (`chain_id = 0`), Istanbul actions MUST only be submitted at `height >= 765432`, except dev/regtest windows documented in `protocol/src/upgrade.rs`.
3. **TEX settlement:** Any transaction containing `TexCellAct` MUST leave the TEX ledger zero-sum; the node calls `do_settlement()` after top-level actions (`protocol/src/transaction/type3.rs`).
4. **TEX signatures:** Each `TexCellAct` MUST be signed over `addr + cells` only (replayable across transactions).
5. **Guard topology:** Bare `GUARD` actions MAY appear at top level alongside other top actions. Transactions MUST NOT be guard-only (`protocol/src/action/level.rs`).
6. **Asset TEX gas:** Transactions with asset transfer TEX cells (`cellid` 7 or 8) MUST set `gas_max > 0` and initialize gas (`extra9` path).

Wallets SHOULD:

- Pre-validate TEX zero-sum pairing off-chain before co-signing counterparty bundles.
- Display guard windows (height, chain, balance floor) in human-readable form.
- Reject co-signed TEX bundles whose `addr` does not match the expected counterparty.

Indexers SHOULD:

- Index `TexCellAct` by signer `addr` and settled asset/diamond deltas.
- Record guard failures as revert reason codes for failed transactions.
- Treat `AssetCreate` + TEX in the same tx as an atomic issuance+distribution event.

## 4. Pattern P1 — Atomic multi-asset TEX swap

### 4.1 Intent

Two or more parties atomically exchange HAC, SAT, diamonds, and/or HIP20 assets in one transaction without trusting an escrow contract.

### 4.2 Structure

1. Party A publishes `TexCellAct_A` with pay cells (`cellid` 1/3/5/7) and/or get cells (`2/4/6/8`).
2. Party B publishes `TexCellAct_B` with the mirrored get/pay cells.
3. A coordinator (or either party) combines both signed bundles in one Type3 transaction.
4. Optional funding actions (e.g. `HacToTrs`, `AssetCreate`) MAY precede TEX actions in the same tx.

### 4.3 Rules

- MUST: For every pay cell in A, B MUST include the matching get cell (same asset serial, amount, coin type).
- MUST: TEX ledger totals for `zhu`, `sat`, `dia`, and each asset serial MUST be zero before `do_settlement()`.
- MUST: Each party signs only their own bundle; tampering after sign MUST fail verification.
- SHOULD: Include TEX condition cells (`cellid >= 11`) when quoting requires balance/height proofs at execution time.

### 4.4 Failure modes

| Failure | Result |
|---------|--------|
| Imbalanced pay/get | Fault at settlement |
| Bad signature | Fault on `TexCellAct` execute |
| Missing asset / insufficient balance | Fault during cell execute |
| Asset cells without gas | Fault (`gas not initialized`) |

## 5. Pattern P2 — Time-boxed guarded payment

### 5.1 Intent

Release a payment only if the transaction is included within a block-height window.

### 5.2 Structure

```
actions: [
  HeightScope { start, end },   // GUARD, top-level
  HacToTrs | SatToTrs | ...     // non-guard debit
]
```

`end = 0` means unlimited upper bound (per `HeightScope` semantics).

### 5.3 Rules

- MUST: `HeightScope` execute before the debit action (lower action index runs first).
- MUST: `start <= end` when `end != 0`.
- MUST: Revert (not fault) when current height is outside `[start, end]`.
- SHOULD: Set `end` to a finite deadline for offer expiry.

### 5.4 Wallet display

Wallets SHOULD show: “Valid heights: `start` … `end` (inclusive)”.

## 6. Pattern P3 — BalanceFloor protected transfer

### 6.1 Intent

Prevent a transaction from leaving an address below a minimum HAC/SAT/diamond/asset balance.

### 6.2 Structure

```
actions: [
  <debit action>,               // e.g. HacToTrs
  BalanceFloor { addr, ... }    // GUARD, checks in-tx state at this point
]
```

`BalanceFloor` intentionally inspects **in-transaction state at the guard position**, not pre-tx chain state.

### 6.3 Rules

- MUST: Place `BalanceFloor` **after** debits that should be protected.
- MUST: Specify at least one non-zero floor field (`hacash`, `satoshi`, `diamond`, or `assets`).
- MUST: Revert when any checked dimension is below floor.
- SHOULD: Floor values include expected post-transfer dust retention.

## 7. Pattern P4 — HIP20 issuance + TEX distribution

### 7.1 Intent

Mint a HIP20 asset and atomically distribute units to counterparties via TEX in the same transaction.

### 7.2 Structure

`AssetCreate` is `TOP_ONLY` — it MUST be the only non-guard top-level action in its transaction.  
Distribution therefore uses **two coordinated transactions**:

```
Tx A (issuance):
  actions: [ AssetCreate { metadata, protocol_cost } ]

Tx B (distribution, after Tx A confirms or in a later pass):
  actions: [
    TexCellAct_issuer  (AssetPay cells),
    TexCellAct_counter (AssetGet cells),
  ]
```

### 7.3 Rules

- MUST: Keep `AssetCreate` alone at top level (`TOP_ONLY` topology).
- MUST: Run TEX distribution only after the asset exists on-chain (same block ordering or later block).
- MUST: `issuer` in metadata receives minted supply and MUST sign issuer `AssetPay` TEX cells.
- MUST: Set `gas_max > 0` on Type3 txs with asset TEX cells.
- MUST: Pay `protocol_cost` per `AssetCreate` rules @ `ASSET_ALIVE_HEIGHT` (`765432`).
- SHOULD: Pre-sign TEX bundles for Tx B before broadcasting Tx A.

## 8. Pattern P5 — AST conditional settlement

### 8.1 Intent

Choose between settlement branches based on an on-chain guard predicate without deploying a contract.

### 8.2 Structure

```
actions: [
  AstIf {
    cond:   AstSelect { HeightScope | BalanceFloor | ... },
    br_if:  AstSelect { HacToTrs | TexCellAct | ... },
    br_else: AstSelect { ... }
  }
]
```

### 8.3 Rules

- MUST: Use Type3 with `gas_max > 0` when AST depth > 0.
- MUST: Condition `AstSelect` use `exe_min = exe_max = 1` for single guard predicates.
- MUST: Guard-only `AstSelect` / `AstIf` nodes are invalid at tx topology precheck.
- MUST: Condition guard `Revert` selects `br_else`; condition `Fault` aborts entire `AstIf`.
- SHOULD: Keep branch actions simple (single transfer or TEX bundle) for wallet simulation.

## 9. Security considerations

- **TEX replay:** Signed TEX bundles are replayable; never treat a signature alone as a one-time authorization.
- **Ordering:** Guards only see state mutations from earlier actions in the same transaction.
- **Co-signing:** Verify counterparty TEX cells match your quote before signing.
- **Gas:** AST and asset TEX paths consume gas; under-budgeted txs fail after partial snapshot work.

## 10. Versioning

| Version | Contents |
|---------|----------|
| v1.0 (this draft) | Patterns P1–P5, JSON templates, regression tests |
| v1.1 (planned) | Wallet checklist, indexer field dictionary |
| v2.0 (future) | Optional HVM companion contracts per pattern |

## 11. Test matrix

| Pattern | Test |
|---------|------|
| P1 | `hip23_pattern_1_atomic_tex_swap` |
| P2 | `hip23_pattern_2_height_guarded_payment` |
| P3 | `hip23_pattern_3_balance_floor_protected_transfer` |
| P4 | `hip23_pattern_4_asset_create_plus_tex` |
| P5 | `hip23_pattern_5_ast_conditional_settlement` |

Run:

```bash
cargo test hip23_pattern -- --nocapture
```