# Hacash CUDA Mining (NVIDIA)

Native CUDA block miner for Hacash, integrated with the existing `poworker` + fullnode RPC stack (same protocol as OpenCL/CPU miners).

**Status:** Kernels compile on Windows (CUDA 12.x/13.x + MSVC Build Tools). GPU runtime validation requires an NVIDIA RTX machine.

**RTX handoff:** [scripts/mining-nvidia/HANDOFF-RTX.md](../scripts/mining-nvidia/HANDOFF-RTX.md) (Greek + English checklist for testers).

## Requirements

- NVIDIA GPU (RTX 20xx / 30xx / 40xx — sm_75 / sm_86 / sm_89)
- [CUDA Toolkit](https://developer.nvidia.com/cuda-downloads) 12.x or 13.x
- Windows: [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/) with **Desktop development with C++** (`cl.exe` for nvcc)
- Rust toolchain (edition 2024)
- Fullnode with `[miner] enable = true`

## Build (Windows)

```bat
scripts\mining-nvidia\BUILD-CUDA-MINER.bat
```

The script auto-detects `CUDA_PATH`, runs `vcvars64.bat`, and builds `target\release\poworker.exe`.

Manual build:

```bat
call "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set CUDA_PATH=C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v13.3
cargo build --release --bin poworker --features cuda
```

Successful kernel build prints: `Using CUDA Toolkit at ...` (no `build without GPU kernels` warning).

## RTX tester handoff checklist

Run on a machine **with NVIDIA GPU**:

```bat
scripts\mining-nvidia\BUILD-CUDA-MINER.bat
scripts\mining-nvidia\TEST-CUDA-GPU.bat
scripts\mining-nvidia\INSTALL-CUDA-CONFIG.bat
scripts\mining-nvidia\START-CUDA-MINING.bat
```

1. **Genesis GPU test** must pass:
   - Expected hash: `000000077790ba2fcdeaef4a4299d9b667135bac577ce204dee8388f1b97f7e6`
2. **poworker startup** must show:
   - `[CUDA] Device #0: ...`
   - `[CUDA] Initialized device #0 work_groups=...`
3. **Mining** against a fullnode with pending work — submit a block or report hashrate logs.

Report back: GPU model, CUDA version, genesis test result, and any `nvcc`/runtime errors.

Example config: `scripts/mining-nvidia/poworker.cuda.ini.example`

## Configure (`poworker.config.ini`)

```ini
[default]
connect = 127.0.0.1:8080
supervene = 4

[gpu]
use_cuda = true
use_opencl = false
cuda_device = 0
work_groups = 131072
unit_size = 8
cpu_assist = true
```

CUDA takes priority over OpenCL when `use_cuda = true`.

## Architecture

| Layer | Path |
|-------|------|
| RPC / work loop | `app/src/poworker.rs` (unchanged protocol) |
| CUDA backend | `app/src/cuda_pow.rs` |
| GPU kernels | `x16rs-cuda/cuda/block_miner.cu` |
| OpenCL reuse | `x16rs/opencl/*.cl` via `ocl_compat.cuh` |

Kernels implement the same x16rs flow as OpenCL: SHA3-256 block intro → x16rs chain → nonce batch search.

## Tests

```bat
cargo test -p x16rs-cuda --features cuda
```

Genesis vector (`x16rs/tests/test.rs`) is cross-checked on GPU when CUDA is available.

## vs community CUDA miners

This miner uses **official fullnode RPC** (`/query/miner/pending`, `/submit/miner/success`). Third-party CUDA pool miners use a different protocol and are not drop-in replacements.