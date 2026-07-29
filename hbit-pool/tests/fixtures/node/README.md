# Node response fixtures

Every file here is a raw HTTP response body captured with `curl` from a running
Hacash full node. Nothing in this directory was hand-written or hand-edited: the
bytes are what the node's own `api_data` / `api_error` serialization produced.
They exist so `hbit-pool/tests/block_fee_holdback_node_fixtures.rs` can drive the
maturity hold-back's fee arithmetic without the pool having to win a block.

## The node

    [Version] full node v1.0.10, build time: 2026/7/10 #1, database type: 8.

An ISOLATED local testnet (`chain_id = 2`, `not_find_nodes`, `not_accept_nodes`),
loopback API only. Testnet and mainnet share the same API and the same `Amount`
serialization, so the `"mantissa:unit"` strings below are exactly what mainnet
renders. Blocks 308 and 309 were mined against that node specifically to produce
these fixtures, with deliberately different transaction fees.

## What each file is

| file | request | why it is here |
| --- | --- | --- |
| `block_307_intro_0tx.json` | `/query/block/intro?height=307&tx_hash_list=true` | a real block with NO transactions: `"transaction":0` and an EMPTY `tx_hash_list`. Proves the node emits the key rather than omitting it, which is what lets the pool tell "no transactions" apart from "field missing". |
| `block_309_intro_3tx.json` | same, `height=309` | a real block with THREE transactions. `"transaction":3` and exactly the three non-coinbase hashes: real evidence that `mint/src/api/block.rs` excludes the coinbase from the list. |
| `tx_309_fee_1_246.json` | `/query/transaction?hash=97a05538...` | `fee_got` = `1:246` = 0.01 HAC |
| `tx_309_fee_3_245.json` | `/query/transaction?hash=3bcaa4a6...` | `fee_got` = `3:245` = 0.003 HAC |
| `tx_309_fee_7_244.json` | `/query/transaction?hash=c2a3f798...` | `fee_got` = `7:244` = 0.0007 HAC. The three sum to 13_700_000 fine steps, a seventh of a payout unit: the block must hold back ONE unit, not three and not zero. |
| `block_308_intro_2tx.json` | same, `height=308` | a real block with TWO transactions. |
| `tx_308_fee_1_247.json` | `/query/transaction?hash=efa4c5fd...` | `fee_got` = `1:247` = 0.1 HAC, exactly one payout unit |
| `tx_308_fee_1_239.json` | `/query/transaction?hash=2ef06a4c...` | `fee_got` = `1:239` = 1e-9 HAC, ONE fine step. The pair sums to 100_000_001 steps, one step past a unit boundary: the case a truncating sum gets wrong. |
| `tx_309_fee_unit_mei.json` | `...hash=97a05538...&unit=mei` | the SAME transaction as `tx_309_fee_1_246.json`, re-fetched in another unit. The node answers `ret:0` with `"fee_got":"0.01"` - a valid body carrying a fee this pool cannot parse. It must stop settlement, never read as zero. |
| `block_missing_error.json` | `...height=999999` | the node's real "cannot find block" error object |
| `tx_missing_error.json` | `...hash=0000...0000` | the node's real "transaction not found" error object |
| `tx_bad_hash_error.json` | `...hash=notahash` | the node's real "transaction hash format invalid" error object |
| `unknown_route_body.txt` | `/query/nonexistent_endpoint` | the body of this node's 404: EMPTY. See `unknown_route_headers.txt`. |
| `unknown_route_headers.txt` | as above | `HTTP/1.1 404 Not Found`, `content-length: 0` |

## Regenerating

Blocks 308 and 309 do not exist on mainnet and never will; they are testnet
blocks mined for this purpose. To regenerate, run a testnet node, mine blocks
carrying transactions with distinct fees, then `curl` the two endpoints above
and save the bodies verbatim. The hashes hard-coded in the test would change
with them.

## What these fixtures do NOT cover

- Mainnet blocks. No mainnet block has ever been won by this pool, so no fixture
  here was produced by the mainnet chain.
- A `fee_got` that is not a whole mantissa at a single unit, and any coinbase
  fee-receiver behaviour, which lives in `protocol/src/block/v1.rs` and is not
  reachable through these two read APIs.
- A block with more transactions than fit one node round trip, or a node that
  answers slowly enough to time out.
