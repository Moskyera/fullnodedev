# JoJoin / official rebuild recipe

Requirement 6: when a new fullnode version ships, the miner should be rebuildable by JoJoin (or any official maintainer) for compatibility.

## Reproducible builds

```bash
git clone https://github.com/hacash/fullnodedev.git   # or Moskyera fork until merged
cd fullnodedev
git checkout <tag-or-commit>

# lockfile required
cargo build --locked --release --features ocl \
  --bin poworker --bin list_opencl --bin diagnose_opencl
cargo build --locked --release --bin hacash --bin diaworker
cargo build --locked --release -p miner-panel
cargo build --locked --release -p miner-pool   # public pool (hac-pool)

# optional NVIDIA CUDA worker
cargo build --locked --release --features cuda --bin poworker
```

## Version alignment

| Component | Must match |
|-----------|------------|
| `protocol` / Istanbul gates | same commit as fullnode |
| `poworker` / `diaworker` | same workspace revision as `hacash` fullnode |
| `hac-pool` | same miner RPC paths as mint API |

## CI

`.github/workflows/release.yml` builds OpenCL workers + panel with `--locked`.  
CUDA package builds need a GPU runner or offline kernel artifact (see mining-nvidia scripts).

## Suggested official process

1. Tag fullnode release `vX.Y.Z`.  
2. Rebuild miner bins from the **same tag**.  
3. Attach miner artifacts to the same GitHub Release (or linked release notes).  
4. List community GPU tools on https://hacash.org/miner when accepted.

Contact: integration request https://github.com/hacash/fullnodedev/issues/9  
