# CUDA on a Google Colab T4

**Runtime -> Change runtime type -> T4 GPU** before anything else. Without it
every cell below either errors or measures nothing.

## The three things you can run, in the order they should be run

| # | What | Command | Proves |
| - | ---- | ------- | ------ |
| 1 | **Byte-equivalence gate** | `bash scripts/mining-nvidia/colab_cuda_gate.sh` | Every hash the card computes equals `x16rs::block_hash`, byte for byte, at repeat 1/4/8/16 across three launch shapes and the production shape. Then that the gate itself catches kernels broken on purpose. |
| 2 | Crate smoke | `bash scripts/mining-nvidia/colab_cuda_smoke.sh` | The `x16rs-cuda` suite: the genesis vector, the differential tests, and the pool share list's bookkeeping (overflow, counter isolation, readback bounds). |
| 3 | Pool end to end | `scripts/mining-nvidia/colab_cuda_pool_e2e.md` | The card is CREDITED in proportion to the work it does, measured against a single CPU thread in the same PPLNS window at real mainnet difficulty. |

**1 comes first and is not optional.** A hashrate from an unproven kernel is a
number, not a result. `colab_cuda_pool_e2e.md` runs the gate as its Cell 2 and
refuses to reach the measurement cells if it fails.

## What the gate costs and how it survives a dying session

| | |
| --- | --- |
| Wall clock, full run | roughly 25 to 45 minutes on a free T4, dominated by four cargo builds |
| Wall clock, `SKIP_FAULTS=1` | roughly a quarter of that, and proves a quarter as much |
| Resumable | yes. Every step writes a marker under `target/gate-state/<fingerprint>/`; a re-run skips what already passed |
| Progress | a heartbeat with elapsed time every 60 seconds during a compile |
| Evidence | `scripts/mining-nvidia/colab-results/gate-*.log` and `latest-gate-summary.txt` |

The fingerprint covers the commit, the kernel sources and the gate sources, so
editing any of them invalidates the markers by itself. `RESUME=0` forces a full
redo.

### Env knobs

| Variable | Effect |
| --- | --- |
| `SKIP_FAULTS=1` | equivalence only, no fault injection. Result becomes `PASS-UNPROVEN` |
| `ALLOW_RACE_MISS=1` | let fault B (a deleted barrier, so a data race) go uncaught. Result becomes `PASS-RACE-NOT-REPRODUCED` |
| `RESUME=0` | ignore markers, redo every step |
| `PROD_WG`, `PROD_UNIT`, `PROD_BATCHES` | production launch shape and how many windows of it. Each window costs a full CPU oracle over `PROD_WG * 256 * PROD_UNIT` nonces |
| `HEADERS` | corpus headers in the exhaustive pass (default 4) |

## Reading the result

`scripts/mining-nvidia/colab-results/latest-gate-summary.txt` is one `key=value`
per line and is the thing to keep. The `result` line is one of:

| `result=` | Meaning |
| --- | --- |
| `PASS` | The kernels match the CPU byte for byte AND the gate caught all three deliberately broken kernels. This is the only clean pass |
| `PASS-RACE-NOT-REPRODUCED` | The kernels match. The arithmetic faults were caught; the data-race fault did not reproduce on this card and was waived. Weaker; quote the string, not the word PASS |
| `PASS-UNPROVEN` | The kernels match, but `SKIP_FAULTS=1` meant the gate was never shown to be able to fail here |
| `FAIL` | Read `reason=`. Either the kernels disagree with the CPU (stop, do not mine), or the device could not be opened (nothing was compared), or a fault went uncaught (the gate cannot be trusted) |

The summary also carries `commit=`, so a log always says which code it is about.

## Getting the code onto Colab

### Option A: clone (needs the gate to be pushed)

```bash
git clone --depth 1 -b feat/pool-directory-cuda-ptx-panel \
    https://github.com/Moskyera/fullnodedev.git /content/fullnodedev
cd /content/fullnodedev
git fetch --depth 1 origin feat/pool-directory-cuda-ptx-panel
git reset --hard FETCH_HEAD
git log --oneline -1
```

The fetch and reset are not redundant. `git clone` is skipped when the directory
already exists, so a session that survived a runtime restart silently re-tests an
old commit.

**Check the clone actually contains the gate, not just the right commit.** The
CUDA half of the gate is newer than the last pushed commit, and at `3248146` none
of it is there:

```bash
grep -l CudaBackend app/src/x16rs_gate.rs
grep -l -- --backend src/bin/x16rs_gate.rs
grep -l X16RS_CUDA_KERNEL_DIR x16rs-cuda/build.rs
grep -l X16RS_H_BLAKE_INIT x16rs/opencl/x16rs.cl
test -f scripts/mining-nvidia/colab_cuda_gate.sh && echo gate-runner-ok
```

All five must print. Cell 1 of `colab_cuda_pool_e2e.md` does this and stops with
the list of what is missing.

`X16RS_H_BLAKE_INIT` deserves its own sentence. It is blake's initialisation
vector, exported from `x16rs.cl` so `block_miner.cu` reads it instead of carrying
its own copy. Without it, fault C (flip one bit of that IV) compiles to
byte-identical PTX, and the CUDA gate returns PASS for a kernel that is broken on
purpose. The whole fault-injection proof rests on it.

### Option B: upload a zip (works with nothing pushed)

This is the only way to run the gate while the CUDA work is still unpushed.

On the Windows box:

```powershell
cd C:\Users\KQHEX\Documents\hacash-fullnodedev
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\mining-nvidia\pack-colab-slim.ps1
```

The packer refuses to build a zip whose tree predates the gate, and writes
`COLAB-PACK-STAMP.txt` with the commit it was packed from. Upload only
`scripts\mining-nvidia\colab-upload\hacash-fullnodedev-colab-slim.zip` (tens of
MB, not the 70 GB working directory, almost all of which is `target/`).

In Colab:

```bash
!unzip -q hacash-fullnodedev-colab-slim.zip -d /content
%cd /content/hacash-fullnodedev
!cat COLAB-PACK-STAMP.txt
!bash scripts/mining-nvidia/colab_cuda_gate.sh
```

Note the directory: the zip unpacks to `/content/hacash-fullnodedev`, while a
clone lands in `/content/fullnodedev`. A zip has no `.git`, so the gate log will
say `commit=not-a-git-checkout`; the pack stamp is what identifies it, so keep
the two together.

## Build profile

Every script here exports the same reduced release profile:

```
CARGO_PROFILE_RELEASE_LTO=false
CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
CARGO_PROFILE_RELEASE_OPT_LEVEL=2
```

The workspace ships `lto = "thin"` with `codegen-units = 1`, which on a free
Colab VM is slow and can be killed for memory. Two consequences worth knowing:

- Cargo keys its build cache on the profile, so **every cell must use the same
  one** or the whole dependency tree compiles twice. `colab_cuda_pool_e2e.md`
  pins it once in Cell 1 for exactly this reason.
- Absolute hashrates from a Colab run are not comparable with a shipping-profile
  build. The GPU kernels are nvcc `-O3` either way, so the CUDA number moves
  little; CPU-side numbers move more. Ratios measured within one run are
  unaffected, because both sides were built the same way.

## FAIL: common causes

| Symptom | Cause and fix |
| --- | --- |
| `no nvidia-smi` | Not a GPU runtime. Runtime -> Change runtime type -> T4 GPU |
| gate exits 4, "the installed NVIDIA driver is older than the CUDA runtime ... (code 35)" | No driver attached. Same fix. This is a clean exit with a message, not a crash |
| gate exits 1, "NO CUDA kernels" | `x16rs-cuda/build.rs` did not find nvcc at build time, so `cfg(cuda_available)` is unset and every device call returns `NotCompiled`. Set `CUDA_PATH=/usr/local/cuda` and rebuild. The gate refuses to run rather than report a pass over zero hashes |
| `cargo test` green but suspiciously fast | `ocl` and `cuda` are optional features. A plain `cargo test` compiles NEITHER backend, and `gpu_share_list_tests` is `#[cfg(all(test, cuda_available))]`, so without nvcc it is not compiled at all and its absence is silent. Check test NAMES in the output, not the pass count |
| Rust edition 2024 error | Update the stable toolchain in the session |
| Session dies mid-build | Re-run. The gate resumes from its markers; a cargo build resumes from `target/` if the VM survived |

## Related files

- `colab_cuda_gate.sh` and `colab_cuda_pool_e2e.md` (start here)
- `colab_cuda_smoke.sh`, `colab_cuda_smoke.ipynb` (the crate suite)
- `pack-colab-slim.ps1` (the zip for Option B)
- `HANDOFF-RTX.md` (Windows RTX checklist)
- `TEST-CUDA-GPU.bat` (Windows equivalent of the crate tests)
