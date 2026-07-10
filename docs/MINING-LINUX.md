# Hacash OpenCL mining on Linux (AMD / Ryzen)

Same **poworker** + **diaworker** + **fullnode** stack as Windows — OpenCL only, no CUDA required.

## Requirements

- **Rust** (edition 2024): [rustup.rs](https://rustup.rs)
- **Build tools**: `build-essential` (gcc, make)
- **OpenCL ICD + headers** at build time: `ocl-icd-opencl-dev` (Debian/Ubuntu) or `ocl-icd-devel` (Fedora)
- **AMD GPU runtime** (one of):
  - [ROCm](https://rocm.docs.amd.com/) (recommended for RX 6000/7000 on Linux)
  - AMDGPU-PRO driver with OpenCL
- **Ryzen CPU** optional (`cpu_assist = true` in config)

> **Note:** Cached OpenCL kernel binaries (`.bin` under `x16rs/opencl/`) are **OS-specific**. A `.bin` from Windows will not load on Linux — first run recompiles kernels (~1–3 min).

> **RX 9070 XT (RDNA4 / `gfx1201`):** The miner code supports this GPU (OpenCL `AMD_GFX_GFX1201`, `work_groups=64`, `unit_size=64`). On Linux you need a **recent ROCm** stack that exposes `gfx1201` via OpenCL (ROCm **6.4+** on Ubuntu 24.04/25.04 or equivalent). This path is validated on Windows; on Linux use **miner-panel** preset **RX 9070 XT** or set WG/US manually — do **not** copy Windows `work_groups=1536` example values.

## Quick start

```bash
git clone https://github.com/Moskyera/fullnodedev.git
cd fullnodedev

# Debian/Ubuntu
sudo apt update
sudo apt install -y build-essential ocl-icd-opencl-dev pkg-config

chmod +x scripts/mining-amd/*.sh
./scripts/mining-amd/build-amd-miner.sh
./scripts/mining-amd/install-configs.sh
./scripts/mining-amd/list-opencl-devices.sh
```

Edit `target/release/poworker.config.ini`:

```ini
[gpu]
use_opencl = true
platform_id = 0      # from list_opencl
device_ids = 0       # GPU index
opencl_dir = ../../x16rs/opencl/
```

### Fullnode + mining

```bash
cp hacash.config.ini target/release/
cd target/release
./hacash &          # RPC on :8080, [miner] enable = true
sleep 35            # node warmup for miner RPC
./poworker          # or: ../../scripts/mining-amd/start-amd-hac-mining.sh
```

HACD diamonds + auto-bids:

```bash
# In hacash.config.ini (fullnode):
#   [miner] enable = false
#   [diamondminer] enable = true
#   reward = 1YourLegacyPrivakey...   # must start with 1 (NOT hybrid 3x...)
#   bid_password = your_bid_wallet_password
#   bid_min = 1:0
#   bid_max = 31:0
#   bid_step = 1:0                  # min step ~1:244 HAC

cp hacash.config.ini target/release/
cd target/release
./hacash &
sleep 35
./diaworker
# or: ../../scripts/mining-amd/start-amd-hacd-mining.sh
```

Fullnode starts `[Diamond Auto Bidding]` when `[diamondminer]` is enabled.

## Scripts (`scripts/mining-amd/`)

| Script | Purpose |
|--------|---------|
| `build-amd-miner.sh` | `cargo build --release --features ocl` |
| `install-configs.sh` | Copy `*.amd.ini.example` → `target/*/poworker.config.ini` |
| `list-opencl-devices.sh` | Show OpenCL platforms/GPUs |
| `start-amd-hac-mining.sh` | Run block miner |
| `start-amd-hacd-mining.sh` | Run diamond miner |

Windows `.bat` equivalents are unchanged — same configs, same `[gpu]` section.

## GPU config

Same presets as Windows — see [MINING-AMD.md](MINING-AMD.md):

- `gpu_profile`: `amd_eco` | `amd_balanced` | `amd_profit` | `amd_performance` | `amd_max`
- `work_groups`, `unit_size`, `cpu_assist`, `[efficiency]` — identical

### Miner Panel (GUI)

Same **miner-panel** as Windows (eframe). Build and run from `target/release/` next to miners:

```bash
# Runtime libs (Debian/Ubuntu desktop)
sudo apt install -y libxcb-render0 libxcb-shape0 libxkbcommon0 libgtk-3-0

./scripts/mining-amd/build-amd-miner.sh      # poworker + hacash first
./scripts/mining-amd/build-miner-panel.sh
./scripts/mining-amd/start-miner-panel.sh
# or: cd target/release && ./miner-panel
```

The panel finds `poworker`, `diaworker`, `hacash` (no `.exe` on Linux). Windows `.exe` builds are unchanged.

**RX 9070 XT in the panel:** select **RX 9070 XT (16GB)** → Save → Start. Panel writes `work_groups=64`, `unit_size=64`, and gfx1201-safe OpenCL paths automatically (same as Windows).

### RX 9070 XT manual `poworker.config.ini` (without panel)

```ini
[gpu]
use_opencl = true
cpu_assist = true
gpu_profile = amd_balanced
platform_id = 0
device_ids = 0
opencl_dir = ../../x16rs/opencl/
work_groups = 64
local_size = 256
unit_size = 64
```

### Linux-specific notes

| Topic | Linux |
|-------|-------|
| GPU temperature | `rocm-smi`, `amd-smi`, or `thermal_file=` path |
| Idle schedule | `date +%H` or UTC fallback |
| miner-panel | `./miner-panel` in `target/release/` (optional) |
| OpenCL path | Forward slashes OK: `../../x16rs/opencl/` |

## Troubleshooting

| Problem | Fix |
|---------|-----|
| `no OpenCL platforms` | Install ROCm or AMDGPU-PRO; check `clinfo` |
| `cannot find -lOpenCL` | `sudo apt install ocl-icd-opencl-dev` |
| `use_opencl=true but no ocl feature` | Rebuild: `./scripts/mining-amd/build-amd-miner.sh` |
| `OpenCL dir not found` | Run from `target/release/` or fix `opencl_dir` |
| Slow first start | Normal — kernel compile; `.bin` cached for next run |
| Low hashrate | Lower `work_groups` if OOM; tune `gpu_profile` |
| `CL_OUT_OF_RESOURCES` on RX 9070 XT | Use `work_groups=64`, `unit_size=64`; rebuild kernels (delete stale `.bin`) |
| `gfx1201` not in `clinfo` | Upgrade ROCm (6.4+) or AMDGPU driver; RDNA4 needs newer runtime than RX 6000/7000 |
| HACD reward rejected (version 7) | Use legacy PRIVAKEY address (`1...`), not hybrid `3x...` |
| `bid step amount cannot be less than 1:244` | Set `bid_step = 1:0` or higher in `[diamondminer]` |

## Package (maintainers)

```bash
./scripts/pack-release-linux.sh v0.4.0
# → dist/hacash-miner-linux-x64-*.tar.gz
```

Contents: `hacash`, `poworker`, `diaworker`, `list_opencl`, `x16rs/opencl/*.cl`, example configs.

## Manual build

```bash
cargo build --release --features ocl
./target/release/list_opencl
./target/release/poworker
./target/release/diaworker
```