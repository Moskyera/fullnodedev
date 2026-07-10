# CUDA miner — RTX tester handoff / Παράδοση σε RTX

**Status:** Kernels compile on Windows (CUDA 12.x/13.x). **GPU runtime not tested here** — needs an NVIDIA RTX machine.

---

## Quick start (English)

1. Clone or pull branch with `x16rs-cuda/` (this repo).
2. Install: [CUDA Toolkit](https://developer.nvidia.com/cuda-downloads), [VS Build Tools](https://visualstudio.microsoft.com/downloads/) (C++), Rust.
3. Build:
   ```bat
   scripts\mining-nvidia\BUILD-CUDA-MINER.bat
   ```
4. Validate GPU (must pass on RTX):
   ```bat
   scripts\mining-nvidia\TEST-CUDA-GPU.bat
   ```
5. Config + run:
   ```bat
   scripts\mining-nvidia\INSTALL-CUDA-CONFIG.bat
   scripts\mining-nvidia\START-CUDA-MINING.bat
   ```

Fullnode must have `[miner] enable = true` and RPC reachable at `connect=` in `poworker.config.ini`.

### Pass criteria

| Step | Expected |
|------|----------|
| Genesis GPU test | Hash `000000077790ba2fcdeaef4a4299d9b667135bac577ce204dee8388f1b97f7e6` |
| poworker startup | `[CUDA] Device #0: NVIDIA GeForce RTX ...` |
| Mining | Hashrate logs, optional block submit via `/submit/miner/success` |

### Report back

Please send: GPU model, driver + CUDA version, genesis test output, poworker first 20 log lines, any errors.

---

## Γρήγορη εκκίνηση (Ελληνικά)

1. Clone / pull το repo με τον φάκελο `x16rs-cuda/`.
2. Εγκατάσταση: CUDA Toolkit, Visual Studio Build Tools (C++), Rust.
3. Build:
   ```bat
   scripts\mining-nvidia\BUILD-CUDA-MINER.bat
   ```
4. Έλεγχος GPU (υποχρεωτικό σε RTX):
   ```bat
   scripts\mining-nvidia\TEST-CUDA-GPU.bat
   ```
5. Ρύθμιση + mining:
   ```bat
   scripts\mining-nvidia\INSTALL-CUDA-CONFIG.bat
   scripts\mining-nvidia\START-CUDA-MINING.bat
   ```

Το fullnode πρέπει να έχει `[miner] enable = true` και το `connect=` στο ini να δείχνει στο RPC.

### Τι να επιβεβαιώσεις

- Genesis test → hash `000000077790ba2fcdeaef4a4299d9b667135bac577ce204dee8388f1b97f7e6`
- Στην εκκίνηση: `[CUDA] Device #0: NVIDIA GeForce RTX ...`
- Κατά το mining: hashrate logs (και block submit αν βρεθεί λύση)

### Τι να στείλεις πίσω

Μοντέλο GPU, έκδοση driver/CUDA, output του TEST-CUDA-GPU, πρώτες 20 γραμμές poworker, τυχόν errors.

---

## Tuning (after genesis passes)

In `poworker.config.ini` `[gpu]` section:

- `work_groups` — grid size (try 65536 → 131072 → 262144)
- `unit_size` — nonces per thread (try 4 → 8 → 16)
- `cpu_assist = true` — hybrid CPU threads alongside CUDA

OOM or CUDA errors → lower `work_groups` or enable `oom_fallback = true` under `[efficiency]`.

---

## vs pool CUDA miners

This uses **official fullnode RPC** (`/query/miner/pending`, `/submit/miner/success`). Community CUDA pool miners (e.g. hacashdot) use a different protocol.

More detail: [docs/MINING-NVIDIA-CUDA.md](../../docs/MINING-NVIDIA-CUDA.md)