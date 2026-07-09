# AMD GPU + Ryzen CPU mining (HAC & HACD)

Official Hacash miners use **OpenCL** (not CUDA). AMD Radeon and Ryzen are supported.

Community miners that only support NVIDIA CUDA are **separate projects** — this stack works on AMD out of the box when built with the `ocl` feature.

## What mines what

| Asset | Worker | Algorithm |
|-------|--------|-----------|
| **HAC** (blocks) | `poworker` | SHA3-256 + x16rs PoW |
| **HACD** (diamonds) | `diaworker` | SHA3-256 + x16rs + diamond filter |

## Quick start (Windows)

1. Install **AMD Adrenalin** drivers (includes OpenCL runtime) or ROCm OpenCL on Linux.
2. Build miners with OpenCL:
   ```bat
   scripts\mining-amd\BUILD-AMD-MINER.bat
   ```
3. Install AMD-tuned configs:
   ```bat
   scripts\mining-amd\INSTALL-CONFIGS.bat
   ```
4. List your GPU platform/device IDs:
   ```bat
   scripts\mining-amd\LIST-OPENCL-DEVICES.bat
   ```
5. Edit `target\release\poworker.config.ini` and `diaworker.config.ini` (copied from `*.amd.ini.example`):
   - `[gpu] platform_id` — usually `0` for AMD on Windows
   - `[gpu] device_ids` — GPU index from step 4
   - `supervene` — Ryzen CPU threads (e.g. `4`–`8`)
6. Run fullnode (`hacash.exe`) with RPC enabled (`[server] enable = true`).
7. Start mining:
   ```bat
   scripts\mining-amd\START-AMD-HAC-MINING.bat
   scripts\mining-amd\START-AMD-HACD-MINING.bat
   ```

## Fullnode config

### HAC block rewards

```ini
[miner]
enable = true
reward = <your legacy address>
```

### HACD diamonds (required for diaworker)

```ini
[diamondminer]
enable = true
reward = <your address>
```

## GPU section reference

```ini
[gpu]
use_opencl = true
cpu_assist = true          ; Ryzen CPU threads + GPU (hybrid)
gpu_profile = amd_performance  ; amd_balanced | amd_performance | amd_max
platform_id = 0
device_ids = 0
opencl_dir = ../../x16rs/opencl/
work_groups = 2048
local_size = 256    ; must stay 256 (kernel requirement)
unit_size = 96
```

### Efficiency presets (`gpu_profile`)

| Profile | work_groups | unit_size | Use when |
|---------|-------------|-----------|----------|
| `amd_balanced` | 1024 | 128 | Default / 8GB VRAM |
| `amd_performance` | 2048 | 96 | RX 6000/7000 — recommended |
| `amd_max` | 4096 | 128 | 12GB+ VRAM, watch GPU temp |

Run `scripts/mining-amd/TUNE-AMD-EFFICIENCY.bat` to set `supervene` from your Ryzen core count.

### Hybrid mining

With `cpu_assist = true`, the GPU runs OpenCL and Ryzen threads mine on CPU **in parallel** — better total hashrate than GPU-only.

## AMD optimizations

When an AMD GPU is detected, the OpenCL compiler enables `amd_bfe` fast paths (`NO_AMD_OPS=0`). Kernel binaries are cached as `DeviceName_<id>_amd.bin` under `x16rs/opencl/`.

## CPU-only (Ryzen, no GPU)

Build without `ocl` or set `use_opencl = false` and increase `supervene` to your core count:

```ini
supervene = 8
```

## Troubleshooting

| Problem | Fix |
|---------|-----|
| `no OpenCL platforms` | Install AMD GPU drivers / OpenCL ICD |
| `use_opencl=true but no ocl feature` | Rebuild with `--features ocl` |
| `OpenCL dir not found` | Run miners from `target/debug` or `target/release`; check `opencl_dir` |
| diaworker idle | Enable `[diamondminer]` on fullnode |
| Low hashrate | Tune `work_groups`; first run compiles kernels (~1 min) |

## Manual build

```bash
cargo build --release --features ocl
./target/release/list_opencl
./target/release/poworker
./target/release/diaworker
```