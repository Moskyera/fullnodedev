# Mainnet HACD (diamond) settings

## Consensus (code - already mainnet)

| Rule | Value |
|------|--------|
| Leading zeros | **10** (`DMD_L`) |
| Name length | **6** after zeros (`DMD_M=16` total string) |
| Mint height | block height **% 5 == 0** only |
| Worker | **CPU only** (OpenCL forced off) |

## Files in this folder

| File | Use as |
|------|--------|
| `hacash.config.mainnet.ini` | `hacash.config.ini` next to `hacash.exe` / `fullnode.exe` |
| `diaworker.mainnet.ini` | `diaworker.config.ini` next to `diaworker.exe` |

## Enable diamond mining on mainnet

### 1. Fullnode

In `hacash.config.ini` (from `hacash.config.mainnet.ini`):

```ini
[diamondminer]
enable = true
reward = 1YourMainnetAddress................
bid_password = your-wallet-password
bid_min = 1
bid_max = 31
bid_step = 1:244
```

- `reward` must be a **PRIVAKEY** address (`1...`).
- `bid_password` is the password of the account that **pays HAC bids**.
- Keep `not_find_nodes = false` and real `boots` so you are on **live** mainnet.

### 2. Diaworker

```ini
connect = 127.0.0.1:8080
supervene = 6
```

GPU section is ignored. Start after the node is up:

```text
diaworker.exe
```

### 3. Flow

1. Node exposes `/query/diamondminer/init` (bid + reward addresses).  
2. Worker mines next diamond number (mainnet difficulty).  
3. Submit mint tx → included only in blocks with `height % 5 == 0`.  
4. Others may bid; your `bid_*` settings drive auto-bidding on the node.

## Local test vs mainnet

| | Local (`not_find_nodes=true`) | Mainnet |
|--|-------------------------------|---------|
| Chain | Fresh / isolated | Live peers |
| Diamond PoW | Same 10-zero rule | Same |
| Practical finds | Easier if chain empty | Hard - real network |

Mainnet diamond difficulty is high; CPU mining is for participation / low hashrate, not guaranteed quick finds.
