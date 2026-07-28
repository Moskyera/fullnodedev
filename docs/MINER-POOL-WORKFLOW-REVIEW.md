# Miner + Pool Bug Review Report

**Repository:** `C:/Users/KQHEX/Documents/hacash-fullnodedev`  
**Scope:** block miner runtime / poworker, GPU OpenCL+CUDA backends, diamond worker + consensus submit  
**Status:** 15 confirmed open bugs (adversarially verified)

---

## 1. Summary

Review of the miner and related GPU/diamond paths confirmed **15 open bugs**. The most severe cluster is in **block mining job lifecycle and winner selection**: same-height reorgs can allow stale in-flight results to out-rank and replace a currently valid solution; depth-reducing reorgs never install a lower pending height; and workers only stop on height *advance* or epoch bump. A second high-severity cluster is in **diamond OpenCL** (uninitialized/wrong reduction seed + LOCAL-only fences on global-backed hashes) and **diamond submit durability** (HTTP-200 noise treated as final; no requeue/save after drain). Medium issues include failed same-height install not retried, late job-switch checks feeding the result channel, SHA3 length mismatch for low diamond numbers, missing GPU medium-hash re-verify, and CUDA batch launch without `clamped_block_size`. Lower-severity items cover nonce-span telemetry after recovery and OpenCL block stuff length acceptance.

---

## 2. Confirmed open bugs

| Severity | File | Issue | Impact |
|---|---|---|---|
| **high** | `app/src/block_mining_runtime.rs:709` | Winners coalesced only by height (keep strongest `result_hash`). After same-height reorg/epoch change, an in-flight stale-template result can still meet its own `target_hash` and out-rank a weaker but currently valid solution; only the stale entry is submitted. No epoch/template id on `BlockMiningResult`; `push_block_mining_success` submits the coalesced winner without live-template revalidation. | A real block acceptable for the current template can be silently discarded; miner loses payout while the node rejects the orphaned solution. |
| **high** | `app/src/poworker.rs:264` | Template install only when `pending_height > curr_hei` or `(same height && intro_changed)`. A reorg that **lowers** pending height is ignored, so `MINING_BLOCK_HEIGHT`/epoch never update and workers never job-switch. | After a depth-reducing reorg the miner can grind a non-existent height indefinitely, producing only rejected work and missing the new tip. |
| **high** | `x16rs/opencl/x16rs_diamond.cl:122` | Per-thread diamond reduction initializes `best_hash = 0` and leaves `best_name` uninitialized, then scans only `i=1..unit_size-1`. Block/CUDA paths already seed from `index` (see `x16rs_main.cl:84`, `block_miner.cu:71-76`). | For `local_id>0`, `best_hash` can point into another thread’s slots; work-group reduction propagates wrong diamond winners—missed finds and inconsistent nonce/hash pairs. |
| **high** | `x16rs/opencl/x16rs_diamond.cl:96` | Diamond kernel stores hashes in global memory (`local_hashes = global_hashes + …`) but post-SHA3 / reduction barriers use only `CLK_LOCAL_MEM_FENCE`. Block kernel uses `CLK_LOCAL_MEM_FENCE \| CLK_GLOBAL_MEM_FENCE` at matching points. | No guaranteed cross-item visibility of global hash writes before reduction; intermittent wrong diamond reductions (lost finds / corrupted best nonce-hash) on some devices. |
| **high** | `app/src/diaworker.rs:890` | `push_diamond_mining_success` treats any HTTP transport `Ok` body as final: breaks immediately, then fails permanently on non-JSON / missing `tx_hash` without retry. Block path (`poworker`) retries unrecognized HTTP-200 bodies; diamond only retries `reqwest` `Err`. | A rare mined diamond can be discarded after proxy/HTML 200, truncated body, or gateway noise within the timeout window. No durable requeue after drain. |
| **medium** | `app/src/poworker.rs:255` | `LAST_PENDING_INTRO` is overwritten **before** `set_pending_block_stuff` succeeds. On same-height install failure (bad target/coinbase/mkrl), the new intro is already remembered → `intro_changed` stays false and install is never retried while height is unchanged. | Miner can remain stuck on an orphaned same-height template until a later height advance, wasting hashrate and missing blocks after reorg/partial RPC payload. |
| **medium** | `app/src/block_mining_runtime.rs:617` | Job-switch (height/epoch) is checked only **after** a full batch and after `send()`. Stale `BlockMiningResult` values always enter the result channel; `deal_block_mining_results` does not filter by current epoch/target before `push_block_mining_success`. Stale submits block the single result thread (HTTP timeouts × attempts). | Feeds same-height winner coalescing failures; delays fresher queue items behind useless submit attempts. |
| **medium** | `app/src/block_mining_runtime.rs:643` | Worker rollover uses `check_hei > mining_hei` (advance-only) plus epoch; correctness for non-monotonic height depends entirely on epoch bumps from `set_pending`, which `pull_pending` may never call on height decrease. | Defense-in-depth gap: if height goes down without an epoch publish path, workers do not stop; compounds the reorg job-switch bug. |
| **medium** | `x16rs/opencl/sha3_256.cl:182` | Host builds 61-byte stuff when custom message is gated empty for diamond numbers ≤ 20000 (`opencl_dia.rs:31-54`), but `sha3_256_hash_diamond` always applies fixed 93-byte SHA3 padding. Consensus/CPU hash true length. | GPU SHA3 diverges from CPU/consensus for low diamond numbers; finds cannot verify—OpenCL diamond mining useless on that range (testnets / early numbers). |
| **medium** | `app/src/opencl_dia.rs:107` | Diamond GPU success path only runs `calculate_hash(stuff)` + `check_diamer_success`; never recomputes `x16rs_hash(repeat, ssshash)` and byte-compares to the GPU medium hash (unlike block `verify_gpu_best_result`). | GPU medium hash can pass independent name/difficulty checks without being the x16rs of the claimed nonce’s SHA3; false local successes under reduction bugs; consensus re-mines and rejects. |
| **medium** | `x16rs-cuda/src/lib.rs:472` | Batch mining launches `x16rs_cuda_main` with fixed `miner.local_size` (256) and never calls `clamped_block_size`. Single-hash path clamps to `maxThreadsPerBlock` to avoid `cudaErrorInvalidConfiguration`. Failures hit 100k-nonce CPU recovery. | If batch kernel `maxThreadsPerBlock` < 256, every CUDA batch fails; hashrate collapses despite a usable GPU. |
| **medium** | `app/src/diaworker.rs:905` | After `MAX_SUBMIT_ATTEMPTS` (5, ~7.5s backoff) network failure, or after a soft parse failure, the drained `DiamondMint` is dropped with only a log—no local save or later resubmit. Recovery curl is commented out. | Node restart, brief RPC outage, or auth blip during submit permanently loses the find even though PoW work completed. |
| **medium** | `x16rs/opencl/x16rs_diamond.cl:122` | Per-work-item reduction leaves `diamond_t best_name` uninitialized and starts comparison at `i=1`, so unit index 0 is never `diamond_hash`’d into `best_name` before comparisons. (Host re-checks candidates; `DiaWorkConf` currently forces `useopencl=false` for HACD.) | If HACD OpenCL is re-enabled, valid nonces can be discarded and hashrate/find rate understated. No false mint while host revalidates and OpenCL is disabled. |
| **low** | `app/src/block_mining_runtime.rs:691` | Drain aggregate adds planned `res.nonce_space` into `total_nonce_space` for the status line even when GPU/CPU recovery reports only a partial window; hashrate EWMA uses partial counts. | Misleading nonce-span / efficiency telemetry after OOM/integrity recovery; can skew operator decisions (not consensus). |
| **low** | `app/src/opencl_gpu/block.rs:20` | OpenCL block upload accepts any stuff length ≤ 512 (`write_stuff_to_gpu`) and never enforces the 89-byte block intro that CUDA requires (`STUFF_BYTES`). Kernel SHA3 assumes fixed 89-byte padded layout. | Short/long intro desyncs GPU vs CPU hashes; integrity verify fails every batch → 100k-nonce CPU recovery until template fixed. Wrong solutions not submitted, but GPU work wasted. |

### Count by severity

| Severity | Count |
|---|---|
| high | 5 |
| medium | 8 |
| low | 2 |
| **total open** | **15** |

### Count by area

| Area | Count |
|---|---|
| block-miner | 6 |
| gpu-backends | 6 |
| diamond-consensus | 3 |

---

## 3. Notes on already-fixed / rejected claims

**Rejected (do not treat as open bugs):**

- **`MINING_BLOCK_HEIGHT` / `EPOCH` Relaxed atomics without Acquire on worker job-switch** (`app/src/block_mining_runtime.rs:316`) — Height/epoch act as Relaxed cancel flags only; template payload is correctly synced via `RwLock`. Missing Acquire/Release does **not** establish an extra real stale-batch bug beyond the confirmed job-switch and coalesce issues above.

**Related “already fixed elsewhere” notes (still open on diamond path):**

- Block OpenCL (`x16rs_main.cl`) and CUDA block miner already seed reduction from `index` and use LOCAL\|GLOBAL fences; diamond OpenCL still has the old reduction/fence pattern.
- Block mining has full `verify_gpu_best_result` recompute; diamond OpenCL success does not recompute/equality-check the medium hash.
- Block submit retries unrecognized HTTP-200 bodies; diamond submit does not.

---

## 4. Recommended fix order

1. **Block reorg job install + worker stop (high, root cause of grinding dead height)**  
   - In `poworker.rs`, install templates when `pending_height != curr_hei` (or explicitly handle `pending_height < curr_hei`), not only advance / same-height intro change.  
   - Always bump `MINING_BLOCK_EPOCH` on any template change including height decrease.  
   - Worker rollover: stop on **any** height change (`check_hei != mining_hei`) or epoch change, not advance-only.

2. **Winner selection / submit revalidation (high, silent payout loss)**  
   - Tag `BlockMiningResult` with epoch (and/or template id / stuff hash).  
   - Coalesce or accept winners only for the **live** epoch/template; re-check result against live `MINING_BLOCK_STUFF` target before `push_block_mining_success`.  
   - Prefer filtering **before** `send()` (or drop in drain) so stale same-height results cannot suppress a live win.

3. **Diamond OpenCL reduction + fences (high, correctness if/when GPU HACD is used)**  
   - Seed `best_hash = index`, hash slot 0 into `best_name` before the loop (mirror `x16rs_main.cl` / CUDA).  
   - Use `CLK_LOCAL_MEM_FENCE \| CLK_GLOBAL_MEM_FENCE` after global hash writes and during reduction.  
   - Keep host revalidation; re-enable only after kernel + SHA3 length fixes.

4. **Diamond submit durability (high → medium)**  
   - Align with block path: retry unrecognized/non-JSON HTTP-200 bodies.  
   - On exhausted attempts or soft parse failure: **local save + requeue** the drained `DiamondMint` (do not only log).  
   - Do not treat every transport `Ok` as terminal success.

5. **Same-height install atomicity (medium)**  
   - Update `LAST_PENDING_INTRO` **only after** successful `set_pending_block_stuff`, or roll back on `Err` so `intro_changed` can retry.

6. **Stale result pipeline (medium)**  
   - Check height/epoch **before** building/sending `BlockMiningResult`.  
   - Drain path: drop winners whose epoch/target no longer match live template before blocking HTTP submit.

7. **Diamond SHA3 length + host verify (medium)**  
   - Make `sha3_256_hash_diamond` length-aware (61 vs 93) consistent with consensus/CPU for numbers ≤ 20000.  
   - Recompute `x16rs_hash` and require equality to GPU medium hash (parity with block `verify_gpu_best_result`).

8. **CUDA batch `clamped_block_size` (medium)**  
   - Launch batch kernel with clamped block size like the single-hash path to avoid permanent invalid-config → 100k CPU recovery collapse.

9. **OpenCL block stuff length (low)**  
   - Enforce 89-byte intro (or reject) on OpenCL upload to match CUDA/`STUFF_BYTES` and fixed kernel pad layout.

10. **Nonce-space telemetry (low)**  
    - Aggregate status `total_nonce_space` from actual recovered `gpu_nonce_space`/`cpu_nonce_space` when partial recovery is reported, not planned `res.nonce_space` alone.

### Suggested patch grouping

| PR | Scope | Severity |
|---|---|---|
| A | Reorg install gate + epoch bump + worker `!=` height stop | high |
| B | Result epoch tag + live-template winner filter + pre-send job check | high |
| C | Diamond CL reduction seed + GLOBAL fences + SHA3 length | high/medium |
| D | Diamond submit retry parity + durable requeue/save | high/medium |
| E | `LAST_PENDING_INTRO` only-after-success; CUDA clamp; OpenCL stuff len; telemetry | medium/low |

---

*End of report — 15 confirmed open bugs; 1 rejected claim.*