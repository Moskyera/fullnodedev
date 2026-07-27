# Hacash CUDA Mining (NVIDIA)

Native CUDA block miner for Hacash, integrated with the existing `poworker` + fullnode RPC stack (same protocol as OpenCL/CPU miners).

## Status (requirement 2)

| Item | Status |
|------|--------|
| Kernels + host (`x16rs-cuda`) | Yes |
| CUDA Toolkit | **12.x / 13.x** |
| GPU arch fatbin | SASS **sm_75** (T4/20xx), **sm_86** (30xx), **sm_89** (40xx) **+ PTX `compute_89`** |
| Newer GPUs (sm_90 Hopper, sm_120 Blackwell/50xx) | **JIT via embedded PTX** (driver compiles at launch; no source edit) |
| Runtime validation | **PASS on NVIDIA Tesla T4** (Google Colab, 2026-07-22); sm_86/89 via fatbin, sm_90+ via PTX (not yet runtime-verified) |
| Tests | `cargo test -p x16rs-cuda --features cuda` → **4 passed** |

Colab free-tier smoke: [scripts/mining-nvidia/COLAB-T4.md](../scripts/mining-nvidia/COLAB-T4.md).

## Requirements

- NVIDIA GPU (T4 / RTX 20xx / 30xx / 40xx)
- [CUDA Toolkit](https://developer.nvidia.com/cuda-downloads) 12.x or 13.x
- Windows: VS Build Tools with C++ (`cl.exe` for nvcc) **or** Linux + nvcc
- Rust toolchain (edition 2024)
- Fullnode with miner API, **or** `hac-pool` in front of it

## Build

### Windows

```bat
scripts\mining-nvidia\BUILD-CUDA-MINER.bat
```

### Linux / Colab

```bash
export CUDA_PATH=/usr/local/cuda
cargo test -p x16rs-cuda --features cuda
cargo build --release --bin poworker --features cuda
```

Successful kernel build logs: `Using CUDA Toolkit at ...`

## Configure (`poworker.config.ini`)

```ini
[default]
connect = 127.0.0.1:8080
; or public pool: connect = POOL_IP:3333

[gpu]
use_cuda = true
use_opencl = false
cuda_device = 0
work_groups = 131072
unit_size = 8
```

## Architecture

| Layer | Path |
|------|------|
| RPC / work loop | `app/src/poworker.rs` |
| CUDA backend | `app/src/cuda_pow.rs` |
| GPU kernels | `x16rs-cuda/cuda/block_miner.cu` |

## vs third-party CUDA pool miners

This miner uses **official fullnode / hac-pool miner RPC**. Closed Stratum-only miners are not drop-in; use `hac-pool` HTTP port with stock `poworker`, or Stratum port with a compatible client.
