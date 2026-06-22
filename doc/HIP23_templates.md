# HIP-23 JSON Templates (Type3)

Templates for wallet builders and integrators. Replace placeholders in `ALL_CAPS`.  
Action JSON uses `kind` (not `type`). TEX cells use `cellid` (not `kind`).

**Mainnet:** submit only at `height >= 765432` unless using dev/regtest gates.

---

## Shared placeholders

| Token | Meaning |
|-------|---------|
| `MAIN_ADDR` | Transaction fee payer / primary signer |
| `PARTY_A` / `PARTY_B` | TEX co-signers |
| `RECIPIENT` | Payment destination |
| `SERIAL` | HIP20 asset serial (u64) |
| `FEE` | Wire amount string, e.g. `"8:244"` |
| `GAS_MAX` | Type3 gas byte, e.g. `17` or `99` when asset TEX present |
| `TIMESTAMP` | Unix seconds |
| `START_HEIGHT` / `END_HEIGHT` | Block height guard window |

---

## P1 — Atomic multi-asset TEX swap

Minimal HAC + HIP20 swap between two parties:

```json
{
  "type": 3,
  "main": "MAIN_ADDR",
  "fee": "FEE",
  "gas_max": 99,
  "timestamp": TIMESTAMP,
  "actions": [
    {
      "kind": 22,
      "addr": "PARTY_A",
      "cells": [
        { "cellid": 1, "haczhu": 100000000 },
        { "cellid": 8, "serial": SERIAL, "amount": 50 }
      ],
      "sign": "PARTY_A_TEX_SIGN"
    },
    {
      "kind": 22,
      "addr": "PARTY_B",
      "cells": [
        { "cellid": 2, "haczhu": 100000000 },
        { "cellid": 7, "serial": SERIAL, "amount": 50 }
      ],
      "sign": "PARTY_B_TEX_SIGN"
    }
  ]
}
```

Extended (HAC + SAT + diamonds) — mirror `tests/tex.rs` `trs1()`:

```json
{
  "kind": 22,
  "addr": "PARTY_A",
  "cells": [
    { "cellid": 2, "haczhu": 100000000 },
    { "cellid": 3, "satnum": 2 },
    { "cellid": 5, "diamonds": "KKKKVA,HYXYHY,UETWNK" },
    { "cellid": 8, "serial": SERIAL, "amount": 100 }
  ],
  "sign": "PARTY_A_TEX_SIGN"
}
```

Counterparty bundle MUST use matching get/pay cell types and amounts.

---

## P2 — Time-boxed guarded payment

```json
{
  "type": 3,
  "main": "MAIN_ADDR",
  "fee": "FEE",
  "gas_max": 0,
  "timestamp": TIMESTAMP,
  "actions": [
    {
      "kind": 1042,
      "start": START_HEIGHT,
      "end": END_HEIGHT
    },
    {
      "kind": 1,
      "to": "RECIPIENT",
      "hacash": "10:244"
    }
  ]
}
```

Notes:

- `kind` `1042` = `HeightScope` (`0x0412`).
- Guard MUST be listed before the debit action.
- `end: 0` = no upper height limit.

---

## P3 — BalanceFloor protected transfer

```json
{
  "type": 3,
  "main": "MAIN_ADDR",
  "fee": "FEE",
  "gas_max": 0,
  "timestamp": TIMESTAMP,
  "actions": [
    {
      "kind": 1,
      "to": "RECIPIENT",
      "hacash": "100:244"
    },
    {
      "kind": 1043,
      "addr": "MAIN_ADDR",
      "hacash": "900:244",
      "satoshi": 0,
      "diamond": 0,
      "assets": []
    }
  ]
}
```

Notes:

- `kind` `1043` = `BalanceFloor` (`0x0413`).
- Floor is evaluated **after** the preceding debit.
- Add `assets: [{ "serial": SERIAL, "amount": MIN_AMT }]` to protect HIP20 balances.

---

## P4 — HIP20 issuance + TEX distribution

`AssetCreate` is `TOP_ONLY`. Use **two transactions**:

**Tx A — issuance**

```json
{
  "type": 3,
  "main": "MAIN_ADDR",
  "fee": "FEE",
  "gas_max": 0,
  "timestamp": TIMESTAMP,
  "actions": [
    {
      "kind": 16,
      "metadata": {
        "serial": SERIAL,
        "supply": 10000,
        "decimal": 2,
        "issuer": "ISSUER_ADDR",
        "ticket": "USDT",
        "name": "Tether"
      },
      "protocol_cost": "1:244"
    }
  ]
}
```

**Tx B — TEX distribution** (after Tx A is valid on-chain)

```json
{
  "type": 3,
  "main": "MAIN_ADDR",
  "fee": "FEE",
  "gas_max": 99,
  "timestamp": TIMESTAMP,
  "actions": [
    {
      "kind": 22,
      "addr": "ISSUER_ADDR",
      "cells": [
        { "cellid": 7, "serial": SERIAL, "amount": 500 }
      ],
      "sign": "ISSUER_TEX_SIGN"
    },
    {
      "kind": 22,
      "addr": "RECIPIENT",
      "cells": [
        { "cellid": 8, "serial": SERIAL, "amount": 500 }
      ],
      "sign": "RECIPIENT_TEX_SIGN"
    }
  ]
}
```

Notes:

- Tx A MUST contain only `AssetCreate` at top level.
- Issuer MUST sign issuer TEX bundle in Tx B.

---

## P5 — AST conditional settlement

Pay only if height guard passes; otherwise no-op branch:

```json
{
  "type": 3,
  "main": "MAIN_ADDR",
  "fee": "FEE",
  "gas_max": 17,
  "timestamp": TIMESTAMP,
  "actions": [
    {
      "kind": 26,
      "cond": {
        "kind": 25,
        "exe_min": 1,
        "exe_max": 1,
        "actions": [
          { "kind": 1042, "start": START_HEIGHT, "end": END_HEIGHT }
        ]
      },
      "br_if": {
        "kind": 25,
        "exe_min": 1,
        "exe_max": 1,
        "actions": [
          { "kind": 1, "to": "RECIPIENT", "hacash": "5:244" }
        ]
      },
      "br_else": {
        "kind": 25,
        "exe_min": 0,
        "exe_max": 0,
        "actions": []
      }
    }
  ]
}
```

Notes:

- `kind` `26` = `AstIf`, `25` = `AstSelect`.
- Condition guard failure (revert) selects `br_else`.
- `gas_max` MUST be non-zero for AST execution.

---

## TEX condition cell reference (optional)

| cellid | Name | Purpose |
|--------|------|---------|
| 11–13 | Zhu at most / at least / eq | HAC balance conditions |
| 14–16 | Sat conditions | SAT balance conditions |
| 17–19 | Diamond conditions | HACD count conditions |
| 20–22 | Asset conditions | HIP20 balance conditions |
| 23–24 | Height at most / at least | Block height conditions |
| 25 | ChainId eq | Chain ID condition |

Example height condition inside a TEX bundle:

```json
{ "cellid": 23, "height": 800000 }
```