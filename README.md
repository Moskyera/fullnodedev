HAC Miner Panel · By Mosky
===

Hacash fullnode + **OpenCL miners** (AMD / NVIDIA / Intel) + **GUI panel** for easy setup.

### Download (no GitHub knowledge needed)

## ⛏️ **[https://moskyera.github.io](https://moskyera.github.io)** two buttons: Full or Miner only

| Package | Use when |
|---------|----------|
| **Full** | Clean PC `hacash.exe` + workers + panel + `SETUP.bat` |
| **Miner only** | You already run the fullnode |

After extract on Windows: **`SETUP.bat`** or **`SETUP-MINER.bat`** → **`miner-panel.exe`**

On Linux x86_64: extract the `.tar.gz` package → **`./SETUP-LINUX.sh`** → **`./START-MINER-PANEL.sh`**

Verify Windows downloads with `Get-FileHash <file>.zip -Algorithm SHA256`; on Linux use `sha256sum -c <file>.tar.gz.sha256`.

### HAC OpenCL + HACD CPU mining

**HAC** uses OpenCL GPUs (AMD/NVIDIA/Intel). CUDA is intentionally not included in the release. **HACD** is CPU/fullnode mining and does not use OpenCL.

See **[docs/MINING-AMD.md](docs/MINING-AMD.md)** (Windows) and **[docs/MINING-LINUX.md](docs/MINING-LINUX.md)** (Linux) — `scripts/mining-amd/` build scripts, GPU configs, `list_opencl` device discovery.

**Maintainers:** pushing a new SemVer tag such as `vX.Y.Z` runs `.github/workflows/release.yml` and builds both Windows ZIPs and both Linux `.tar.gz` archives.

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

