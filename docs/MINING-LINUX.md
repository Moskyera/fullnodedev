# Hacash mining on Linux (OpenCL)

The same beginner-friendly panel is supported on 64-bit Linux. Linux releases are
built and tested on Ubuntu by GitHub Actions even when the maintainer is working
from Windows.

## What uses the GPU?

- **HAC:** OpenCL GPU mining through `poworker`. CUDA is not used.
- **HACD:** CPU/fullnode mining through `diaworker`. OpenCL, GPU presets and GPU
  Auto Tune do not apply to HACD. The HACD worker and fullnode are deliberately
  built without a `libOpenCL` dependency.
- **Miner Fleet:** works over the same read-only LAN API on Windows and Linux.

This follows the mining split documented by Hacash:
[HAC miner](https://hacash.org/mining-HAC) and
[HACD miner](https://hacash.org/mining-HACD).

## Recommended: prebuilt Linux release

Download one of the Linux x86_64 archives from GitHub Releases:

| Archive | Use it when |
|---|---|
| `hacash-miner-full-linux-x86_64*.tar.gz` | New PC; includes fullnode |
| `hacash-miner-only-linux-x86_64*.tar.gz` | A fullnode already runs on port 8080 |

Download the matching `.sha256` file and verify the archive before extraction:

```bash
sha256sum -c hacash-miner-full-linux-x86_64*.tar.gz.sha256
```

The prebuilt compatibility baseline is Ubuntu 22.04+ or Debian 12+ on x86_64.
Other modern distributions may work; build from source when their runtime is older.

Then:

```bash
tar -xzf hacash-miner-full-linux-x86_64*.tar.gz
cd hacash-miner-full-linux-x86_64
./SETUP-LINUX.sh
# If executable permissions were lost:
# bash SETUP-LINUX.sh
```

The setup:

1. checks that the required binaries and OpenCL kernel files are present;
2. creates safe HAC and CPU-only HACD configuration files;
3. offers to install standard Ubuntu/Debian runtime libraries when missing;
4. runs OpenCL detection;
5. creates a desktop launcher and opens the panel when a desktop session exists.

Both archives include PRESETS-INDEX.txt and matching HAC/HACD preset files for
manual inspection. HACD copies remain explicitly CPU-only.

After setup, use `./START-MINER-PANEL.sh` or the generated
`HAC-Miner-Panel.desktop` launcher.

## GPU driver requirement

The release can supply the standard OpenCL loader, but it cannot safely choose
and install the correct vendor driver for every GPU.

For AMD Radeon, use the current official
[ROCm Linux installation guide](https://rocm.docs.amd.com/projects/install-on-linux/en/latest/)
and install the OpenCL runtime. Confirm the result with:

```bash
./list_opencl
```

The official AMD compatibility matrix currently includes RX 9070 / RX 9070 XT.
Use an Ubuntu/kernel/ROCm combination listed in the current
[Radeon Linux support matrix](https://rocm.docs.amd.com/projects/radeon-ryzen/en/latest/docs/compatibility/compatibilityrad/native_linux/native_linux_compatibility.html).

OpenCL kernel cache files are OS- and driver-specific. Do not copy Windows
`.bin` cache files to Linux. The first HAC start can spend a few minutes
compiling kernels; later starts use the Linux cache.

Thermal protection is off by default. If `max_temp_c` is enabled, the selected GPU must have a working `amd-smi`/`rocm-smi`, `nvidia-smi` or explicit `thermal_file` backend. The miner fails closed rather than pretending an unrelated sensor belongs to the GPU.

## Auto Tune on Linux

Auto Tune is available for HAC once `diagnose_opencl` reports a usable GPU.
It benchmarks safe work-group and unit-size candidates for the detected device,
then saves the best stable result. RX 9070 / gfx1201 safety limits are shared
with the Windows build.

One `poworker` config may Auto Tune one selected GPU. For multiple or heterogeneous cards, run one instance/config per GPU and aggregate their panels with Miner Fleet.

Auto Tune cannot be validated by a CI virtual machine because GitHub's runner
has no physical OpenCL GPU. CI verifies compilation, unit tests, packaging and
GUI startup; a real Linux GPU is still required for final hashrate and thermal
validation.

## Multiple miners in one dashboard

On every remote Linux or Windows panel:

1. open **Settings -> Miner Fleet**;
2. enable LAN sharing;
3. copy the shown address, port and access token.

On the main panel open **Dashboard -> Manage miners**, add each remote miner and
save. The dashboard totals online miners, hashrate, power and estimated daily
cost. The Fleet service is read-only and disabled by default.

Allow the selected Fleet TCP port through the local firewall only on the trusted
LAN. Do not expose it directly to the internet.

## Build from source

Ubuntu build dependencies:

```bash
sudo apt update
sudo apt install -y \
  build-essential pkg-config ocl-icd-opencl-dev libssl-dev \
  libx11-dev libxkbcommon-dev libwayland-dev libgl1-mesa-dev \
  libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libudev-dev

# Install Rust stable from https://rustup.rs, then:
cargo build --release --features ocl \
  --bin poworker --bin list_opencl --bin diagnose_opencl
cargo build --release --bin hacash --bin diaworker
cargo build --release -p miner-panel
```

Or use the repository helpers:

```bash
bash scripts/mining-amd/build-amd-miner.sh
bash scripts/mining-amd/build-miner-panel.sh
bash scripts/mining-amd/install-configs.sh
```

Maintainers can build both release archives with:

```bash
bash scripts/pack-release-linux.sh v1.0.0
```

## Troubleshooting

| Problem | Fix |
|---|---|
| `libOpenCL.so.1` missing | Run `SETUP-LINUX.sh` or install `ocl-icd-libopencl1` |
| No OpenCL platforms | Install the vendor OpenCL runtime, then run `./list_opencl` |
| RX 9070 not shown | Use a supported current Ubuntu/kernel/ROCm combination |
| Panel does not open | Run it from a terminal and install the GUI runtime packages offered by setup |
| Slow first HAC start | Normal kernel compilation; wait and keep the generated Linux cache |
| `CL_OUT_OF_RESOURCES` | Run Auto Tune; remove stale cache after a driver change |
| Thermal sensor unavailable | Leave `max_temp_c = 0`, or install/configure an exact supported sensor backend before enabling the limit |
| HACD shows no GPU | Correct: HACD is CPU-only; increase CPU threads in the panel |
| Fullnode not ready | Wait for sync and verify RPC is listening on `127.0.0.1:8080` |
