# AMD GPU + Ryzen CPU mining (HAC & HACD)

This Mosky HAC miner uses **OpenCL**. AMD Radeon, NVIDIA and Intel OpenCL devices are supported; CUDA is intentionally not part of the release.

HACD mining is CPU/fullnode-only and never uses OpenCL.

## What mines what

| Asset | Worker | Hardware path |
|-------|--------|---------------|
| **HAC** (blocks) | `poworker` | OpenCL GPU, with optional CPU assist |
| **HACD** (diamonds) | `diaworker` | CPU/fullnode only |

## Quick start (Linux)

See **[MINING-LINUX.md](MINING-LINUX.md)** for full Debian/Ubuntu/ROCm setup.

```bash
chmod +x scripts/mining-amd/*.sh
./scripts/mining-amd/build-amd-miner.sh
./scripts/mining-amd/install-configs.sh
./scripts/mining-amd/list-opencl-devices.sh
./scripts/mining-amd/start-amd-hac-mining.sh
```

## Quick start (Windows)

**End users (GitHub Releases):**

- **`hacash-miner-full-windows-x64*.zip`** — clean PC: fullnode + miners + panel → run `SETUP.bat`
- **`hacash-miner-only-windows-x64*.zip`** — you already have fullnode → run `SETUP-MINER.bat`

**Maintainers:** push a new SemVer tag such as `vX.Y.Z`, or run **Actions → Release (Windows + Linux miners) → Run workflow** for artifacts without publishing a tagged release.

1. Install a current **AMD Adrenalin** driver with its OpenCL runtime.
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
5. Configure the workers (or use the panel):
   - HAC `poworker.config.ini`: set OpenCL `platform_id` / `device_ids`
   - HACD `diaworker.config.ini`: set CPU `supervene`; GPU keys stay disabled
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
reward = YOUR_HACD_PRIVAKEY_3x
```

The HACD reward value is a private key that starts with `3`. Keep it secret and never paste it into Fleet peers, logs or support messages.

HACD mining is CPU/fullnode-only. In `diaworker.config.ini`, keep
`use_opencl = false` and use `supervene` to select CPU threads.

## HAC GPU section reference

The packaged first-run baseline is deliberately conservative. The panel detects the actual GPU, applies its architecture limits and Auto Tune measures the selected mode.

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

### Nominal profile candidates (`gpu_profile`)

These are generic starting candidates. Runtime device limits always win, and Auto Tune writes the exact measured values.

| Profile | Nominal work_groups | Nominal unit_size | Purpose |
|---------|---------------------|-------------------|---------|
| `amd_eco` | 768 | 128 | Low-power candidate |
| `amd_balanced` | 1024 | 128 | General AMD baseline |
| `amd_profit` | 1536 | 96 | Efficiency candidate |
| `amd_performance` | 2048 | 96 | Performance candidate for older RX architectures |
| `amd_max` | 4096 | 128 | Aggressive generic candidate; use only through Auto Tune |

**RX 9070 XT / gfx1201:** validated hard ranges are `work_groups 32–64` and `unit_size 32–64`. Do not copy the generic 2048/4096 values into an RDNA4 config. First-run detection maps `gfx1201` to the RX 9070 XT preset before mining starts.

Auto Tune is fail-closed for more than one selected GPU because one INI currently stores one shared WG/US pair. Use one `poworker` instance/config per GPU—especially for heterogeneous cards—and aggregate them with Miner Fleet.

### Cost-aware mining (`[efficiency]`)

```ini
[efficiency]
mode = profit              ; eco | profit | max
power_cost_kwh = 0.15
gpu_watts = 0              ; 0 = estimate from profile
hac_price = 0              ; set for estimated net EUR/day
dynamic_supervene = true
supervene_min = 2
supervene_max = 10
oom_fallback = true
max_temp_c = 0             ; off by default
throttle_work_groups = 32
thermal_gpu_index = 0
thermal_file =
idle_start_hour = 255
benchmark_seconds = 0      ; the panel controls Auto Tune
```

When `max_temp_c > 0`, a valid `amd-smi`/`rocm-smi`, `nvidia-smi` or explicit `thermal_file` sensor is required. Mining refuses to start if thermal protection is enabled without an exact sensor. After three missed samples the miner applies a conservative cap; at `max_temp + 5°C` it pauses until hysteresis recovery.

Hashrate units are selected automatically (`H/s`, `kH/s` or `MH/s`). Watts, kH/J and daily values are estimates unless an external telemetry source is supplied.

Scripts:
- `CONFIGURE-MINING.bat` — pick CPU + GPU
- `BENCHMARK-AMD.bat` — run the benchmark helper

Use the panel Auto Tune for GPU WG/US values. `TUNE-AMD-EFFICIENCY.bat` only adjusts basic CPU `supervene` settings.

### HAC hybrid mining

With `cpu_assist = true`, the GPU runs OpenCL and Ryzen threads mine on CPU **in parallel** — better total hashrate than GPU-only.

## AMD optimizations

When AMD is detected, the OpenCL compiler enables the `amd_bfe` path (`NO_AMD_OPS=0`). Cache names include the sanitized device, device id, architecture and optional diamond suffix: `DeviceName_<id>_<arch>[_dia].bin`.

Every gfx1201 startup runs a Groestl integrity self-test, and every production GPU result is CPU-verified before submission.

## HAC CPU-only fallback (Ryzen, no GPU)

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
| Low or unstable hashrate | Let the first kernel compile finish, then run panel Auto Tune; after a driver update remove only the stale `.bin` cache |

## Manual build

Build HAC OpenCL tools and HACD/fullnode separately so `hacash` and `diaworker` remain CPU-only:

```bash
cargo build --locked --release --features ocl \
  --bin poworker --bin list_opencl --bin diagnose_opencl
cargo build --locked --release --bin hacash --bin diaworker
cargo build --locked --release -p miner-panel

./target/release/list_opencl
./target/release/poworker
# HACD only after [diamondminer] is configured:
./target/release/diaworker
```
