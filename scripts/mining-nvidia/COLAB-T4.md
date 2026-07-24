# CUDA Phase 1 — Google Colab T4

**Goal:** prove CUDA kernels + `poworker --features cuda` on a real NVIDIA GPU (T4) without a local card.

**Rules:** no GitHub push until this smoke is **PASS** and you keep the log files.

## Free tier vs full

| Mode | Command | Time (typical) | Enough for Phase 1? |
|------|---------|----------------|---------------------|
| **FREE (default)** | `bash scripts/mining-nvidia/colab_cuda_smoke.sh` | ~10–25 min | **Yes** — CUDA kernels + tests |
| FULL (optional) | `COLAB_FULL=1 bash ...` | 30–90+ min | Extra: poworker binary |

**Do not** run the old full `cargo build --release --bin poworker` on free Colab with workspace `lto = true` — it can look like it never finishes and the session dies.

Heartbeat lines every 60s (`still working...`) mean it is alive.

## PASS checklist (FREE default)

| # | Check | How |
|---|--------|-----|
| 1 | GPU visible | `nvidia-smi` (T4 OK) |
| 2 | Toolkit | `nvcc --version` |
| 3 | Unit tests | `cargo test -p x16rs-cuda --features cuda` exit 0 |
| 4 | Genesis + CPU/GPU + batch | included in those tests |
| 5 | Summary | `result=PASS` in `colab-results/latest-summary.txt` |

poworker release build is **optional** (`COLAB_FULL=1`).

## Colab steps

1. Open [Google Colab](https://colab.research.google.com/).
2. **Runtime → Change runtime type → T4 GPU**.
3. **Do NOT upload the full 70GB folder.** Almost all of that is `target/` (Rust build cache), not source.
   - On your PC run:
     ```powershell
     cd C:\Users\KQHEX\Documents\hacash-fullnodedev
     powershell -NoProfile -ExecutionPolicy Bypass -File scripts\mining-nvidia\pack-colab-slim.ps1
     ```
   - Upload only:
     `scripts\mining-nvidia\colab-upload\hacash-fullnodedev-colab-slim.zip`
     (usually tens of MB, not GB)
   - In Colab:
     ```bash
     !unzip -q hacash-fullnodedev-colab-slim.zip -d /content
     %cd /content/hacash-fullnodedev
     !bash scripts/mining-nvidia/colab_cuda_smoke.sh
     ```
   - **Or** (if Phase 1 is already on GitHub): `git clone --depth 1` your fork (small) — Colab rebuilds `target/` on the VM.
4. Run the notebook cells, or:
   ```bash
   cd /content/fullnodedev
   chmod +x scripts/mining-nvidia/colab_cuda_smoke.sh
   bash scripts/mining-nvidia/colab_cuda_smoke.sh
   ```
5. First compile is slow (often 15–40 min). Keep the tab open.
6. Download `scripts/mining-nvidia/colab-results/*` before the session dies.

## Evidence files

After a run:

- `scripts/mining-nvidia/colab-results/smoke-*.log`
- `scripts/mining-nvidia/colab-results/latest-summary.txt` (`result=PASS` or `FAIL`)

## FAIL common causes

| Symptom | Fix |
|---------|-----|
| No `nvidia-smi` | Enable T4 GPU runtime |
| `CUDA Toolkit not found` | Script sets `/usr/local/cuda`; re-run after GPU attach |
| Kernels not compiled | Confirm cargo warning: `Using CUDA Toolkit at ...` |
| Rust edition 2024 error | Update rustup stable in the script/session |
| Session timeout | Re-run; use Colab Pro if free tier kills long builds |

## After PASS

1. Keep logs offline.
2. Only then update docs / RC notes with real T4 evidence.
3. Still **no production pool/Stratum claim** from Phase 1 alone.
4. Still **no push** until you decide the tree is ready (your rule).

## Related

- `colab_cuda_smoke.sh` — automated checks
- `colab_cuda_smoke.ipynb` — Colab notebook
- `HANDOFF-RTX.md` — Windows RTX checklist
- `TEST-CUDA-GPU.bat` — Windows equivalent of tests
