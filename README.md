Hacash Fullnode
===

### AMD / Ryzen mining (HAC + HACD)

Official miners use **OpenCL** (AMD Radeon + Ryzen CPU). Not CUDA.

See **[docs/MINING-AMD.md](docs/MINING-AMD.md)** and `scripts/mining-amd/` for build scripts, GPU configs, and `list_opencl` device discovery.

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

