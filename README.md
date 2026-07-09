HAC Miner Panel · By Mosky
===

Hacash fullnode + **OpenCL miners** (AMD / NVIDIA) + **GUI panel** for easy setup.

### Downloads (GitHub Releases)

| Package | Use when |
|---------|----------|
| **`hacash-miner-full-windows-x64.zip`** | Clean PC — includes `hacash.exe`, workers, panel, `SETUP.bat` |
| **`hacash-miner-only-windows-x64.zip`** | You already run the fullnode — workers + panel only |

After extract: run **`SETUP.bat`** (full) or **`SETUP-MINER.bat`** (miner-only), then **`miner-panel.exe`**.

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

