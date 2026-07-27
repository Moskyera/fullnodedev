# Community miner requirements — status

Target list from community / jojoin discussion:

1. Stratum and free IP pool  
2. Including new versions of CUDA  
3. Integration with open source and official libraries  
4. Diaworker + Poworker  
5. Anyone can broadcast a public pool of content  
6. JoJoin rebuilds miner when fullnode updates  

## Status matrix

| # | Requirement | Status | How |
|---|-------------|--------|-----|
| 1 | Stratum + free IP pool | **Implemented (v1)** | Binary `hac-pool`: HTTP free-IP bind + Stratum TCP |
| 2 | New CUDA versions | **Implemented (validated)** | CUDA 12/13, sm_75/86/89; T4 Colab PASS |
| 3 | Official open-source libs | **Yes (community fork)** | Based on `hacash/fullnodedev`; integration request open |
| 4 | Diaworker + Poworker | **Yes** | Both in packages and builds |
| 5 | Public pool broadcast | **Implemented (v1)** | Anyone runs `hac-pool` on 0.0.0.0; workers connect |
| 6 | JoJoin rebuild | **Process ready** | See [JOJOIN-REBUILD.md](JOJOIN-REBUILD.md); needs org ownership |

## Quick start public pool (1 + 5)

```bash
# 1) fullnode with miner API (loopback OK)
# 2) free-IP pool in front of it
cargo build --release -p miner-pool
./target/release/hac-pool \
  --upstream 127.0.0.1:8080 \
  --http-bind 0.0.0.0:3333 \
  --stratum-bind 0.0.0.0:3334

# 3) workers (existing poworker) point at the pool IP
# poworker.config.ini:
#   connect = YOUR_PUBLIC_IP:3333
```

Optional: `--pool-token SECRET` then workers send `api_token` / Stratum password.

## CUDA (2)

- Docs: [MINING-NVIDIA-CUDA.md](MINING-NVIDIA-CUDA.md)  
- Colab smoke: `scripts/mining-nvidia/colab_cuda_smoke.sh`  
- Evidence: Tesla T4, `cargo test -p x16rs-cuda --features cuda` → 4 passed  

## Official integration (3 + 6)

- Issues: https://github.com/hacash/fullnodedev/issues/9  
- Rebuild recipe: [JOJOIN-REBUILD.md](JOJOIN-REBUILD.md)  

## Honest limits (v1)

- Pool is a **work proxy** (official miner RPC + minimal Stratum).  
- No share accounting / PPS / wallet payouts yet (can be added later).  
- Stratum is **Hacash-oriented** (job carries `block_intro` + height); not a drop-in for every third-party closed miner.  
- Existing **poworker** uses HTTP pool port (not Stratum) for zero worker code change.  
