# Mainnet templates: HAC block mining and HACD (diamond) mining

## Consensus (code - already mainnet)

| Rule | Value |
|------|--------|
| Genesis | `000000077790ba2fcdeaef4a4299d9b667135bac577ce204dee8388f1b97f7e6` |
| Leading zeros (HACD) | **10** (`DMD_L`) |
| Name length (HACD) | **6** after zeros (`DMD_M=16` total string) |
| Mint height (HACD) | block height **% 5 == 0** only |
| HACD worker | **CPU only** (OpenCL forced off) |

## Files in this folder

| File | Use as |
|------|--------|
| `hacash.config.mainnet.ini` | `hacash.config.ini` next to `hacash.exe` / `fullnode.exe` |
| `poworker.mainnet.ini` | `poworker.config.ini` next to `poworker.exe` (HAC, GPU) |
| `diaworker.mainnet.ini` | `diaworker.config.ini` next to `diaworker.exe` (HACD, CPU) |

## Read this before you copy anything

Every template ships with **both miners off and no reward address**. That is
deliberate. A reward address you did not type yourself pays every block reward
to whoever owns it, permanently, with no error message. There is no built-in
fallback address anywhere in the node: if a miner is enabled and `reward` is
missing, the node prints a config error and stops.

## 1. Fullnode

Copy `hacash.config.mainnet.ini` next to `hacash.exe` as `hacash.config.ini`.

To mine **HAC blocks**, make two edits in `[miner]`:

```ini
[miner]
enable = true
reward = 1YourOwnMainnetAddress
message = hacashminer
```

To mine **HACD diamonds**, make the matching edits in `[diamondminer]`:

```ini
[diamondminer]
enable = true
reward = 1YourOwnMainnetAddress
bid_password = your-wallet-password
bid_min = 1
bid_max = 31
bid_step = 0.5
```

- `reward` must be a **PRIVAKEY** address. That is the ordinary kind and it
  starts with `1...`. A non-PRIVAKEY address is rejected at startup.
- `bid_password` is the password of the account that **pays the HAC bids**, so
  that account must hold HAC. It must not be the well known `123456`: that
  private key is public, and the node refuses to start with it.
- Bid amounts are **plain HAC**: `1` is 1 HAC, `0.5` is half a HAC,
  `0.0001` is the smallest step the auto-bidder accepts. Do not use the
  `mantissa:unit` form (`1:244`) here.
- Keep `not_find_nodes = false` and real `boots` so you are on **live** mainnet.
  With `not_find_nodes = true` the node builds an isolated chain from height 0,
  X16RS runs at repeat = 1, and the MH/s you see is about 16x the real rate.

## 2. HAC GPU miner (poworker)

Copy `poworker.mainnet.ini` as `poworker.config.ini` next to `poworker.exe`,
then run `poworker.exe`. It connects to `127.0.0.1:8080`. Run Auto Tune
(`benchmark_seconds > 0`) once to fit the `[gpu]` sizes to your card.

To measure the honest mainnet rate without waiting for a full sync:

```powershell
$env:HACASH_REPEAT16_BENCH_SECONDS = "30"
.\poworker.exe
Remove-Item Env:\HACASH_REPEAT16_BENCH_SECONDS
```

## 3. HACD CPU miner (diaworker)

```ini
connect = 127.0.0.1:8080
supervene = 6
```

The `[gpu]` section is ignored: diamonds are CPU-only and enforced as such in
code. Start after the node is up:

```text
diaworker.exe
```

## 4. Diamond flow

1. Node exposes `/query/diamondminer/init` (bid + reward addresses).
2. Worker mines the next diamond number (mainnet difficulty).
3. Submit mint tx, included only in blocks with `height % 5 == 0`.
4. Others may bid; your `bid_*` settings drive auto-bidding on the node.

## Local test vs mainnet

| | Local (`not_find_nodes=true`) | Mainnet |
|--|-------------------------------|---------|
| Chain | Fresh / isolated | Live peers |
| X16RS rounds | repeat = 1 (inflated MH/s) | repeat = 16 (real) |
| Diamond PoW | Same 10-zero rule | Same |
| Practical finds | Easier if chain empty | Hard - real network |

Mainnet diamond difficulty is high; CPU mining is for participation / low
hashrate, not guaranteed quick finds.

Boot nodes rotate over time. If the node finds no peers, get current peers from
the community and update `boots =`.
