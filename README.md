HAC Miner Panel · By Mosky
===

Hacash fullnode + **OpenCL miners** (AMD / NVIDIA) + **GUI panel** for easy setup.

### Download (no GitHub knowledge needed)

## ⛏️ **[https://moskyera.github.io](https://moskyera.github.io)** — two buttons: Full or Miner only

| Package | Use when |
|---------|----------|
| **Full** | Clean PC — `hacash.exe` + workers + panel + `SETUP.bat` |
| **Miner only** | You already run the fullnode |

After extract: **`SETUP.bat`** or **`SETUP-MINER.bat`** → **`miner-panel.exe`**

### AMD / Ryzen mining (HAC + HACD)

Official miners use **OpenCL** (AMD Radeon + Ryzen CPU). Not CUDA.

See **[docs/MINING-AMD.md](docs/MINING-AMD.md)** and `scripts/mining-amd/` for build scripts, GPU configs, and `list_opencl` device discovery.

**Maintainers:** `git tag v0.4.0 && git push origin v0.4.0` → GitHub Actions builds both ZIPs (`.github/workflows/release.yml`).

### Module Architecture

```
- x16rs    ->  x16rs-sys
- sys      ->  -
- field    ->  sys
- basis    ->  field
- protocol ->  basis
- chain    ->  protocol
- scaner   ->  protocol
- mint     ->  protocol
- node     ->  protocol, tokio
- server   ->  mint, tokio
- app      ->  mint
```

