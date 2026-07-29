# Delta Bug Review Report

**Repo:** `C:/Users/KQHEX/Documents/hacash-fullnodedev`  
**Scope:** Block miner, diamond GPU, hacd-pool / miner-pool  
**Date:** 2026-07-24

---

## 1. Summary

| Category | Count |
|----------|------:|
| **Fixed** | 5 |
| **Still open** | 14 |
| **Unclear** | 0 |
| **New** | 11 |

**Net residual risk:** 25 open issues (14 prior + 11 new). Critical consensus/testnet hacks (**C1**, **C2**) and the major block-mining correctness suite (**C3**, **C4**, **P11**) are fixed. Remaining work clusters on **template/reorg lifecycle**, **OpenCL diamond kernel correctness**, and **submit durability**.

---

## 2. Fixed

| ID | Severity | File | Note |
|----|----------|------|------|
| **P11** | medium | `x16rs-cuda` batch launch | Batch path calls `clamped_block_size` at `cuda_init_miner` and refuses devices that cannot launch 256-thread blocks. |
| **C1** | critical | `x16rs/src/diamond.rs` | Mainnet `DMD_L=10` / `DMD_M=16` hardcoded; no testnet 4/10 override. |
| **C2** | critical | `mint/src/action/diamond_mint.rs` | Height `% 5` diamond mint rule enforced; no `if false` bypass. |
| **C3** | high | `app/src` mining winners / equal target / GPU fatal | Multi-winner submit, equal-inclusive target, logged GPU fail, CUDA verify, capped CPU recovery — implemented and unit-tested. |
| **C4** | high | `miner-pool` `rpc_proxy` / `stratum` | Non-JSON upstream → `ret:1`; stratum accepts only parseable JSON with `ret==0`. |

---

## 3. Still open

| ID | Severity | File | Issue |
|----|----------|------|-------|
| **P1** | high | `app/src/block_mining_runtime.rs` | Winners coalesced without epoch/template id; submit/drain skip live-template revalidation; same-height reorg stale result can out-rank live work. |
| **P2** | high | `app/src/poworker.rs` | Template install only on height advance or same-height `intro_changed`; reorg that **lowers** pending height is ignored. |
| **P3** | high | `x16rs/opencl/x16rs_diamond.cl` | Diamond OpenCL reduction seeds `best_hash=0` with uninitialized `best_name`; scans `i=1..` (unlike block kernel seed-from-index). |
| **P4** | high | `x16rs/opencl/x16rs_diamond.cl` | Diamond barriers use only `CLK_LOCAL_MEM_FENCE` while hashes live in global memory; block kernel uses `LOCAL\|GLOBAL`. |
| **P5** | high | `app/src/diaworker.rs` | `push_diamond_mining_success` treats any HTTP Ok as final; non-JSON / missing `tx_hash` fails permanently; no requeue after drain. |
| **P6** | medium | `app/src/poworker.rs` | `LAST_PENDING_INTRO` written **before** `set_pending_block_stuff` succeeds; failed same-height install never retried. |
| **P7** | medium | `app/src/block_mining_runtime.rs` | Job-switch height/epoch checked only **after** full batch + `send()`; drain does not filter by live epoch. |
| **P8** | medium | `app/src/block_mining_runtime.rs` | Worker rollover is advance-only (`check_hei > mining_hei`) + epoch; height decrease without epoch may not stop workers. |
| **P9** | medium | `x16rs/opencl/sha3_256.cl` | Host builds 61-byte diamond stuff for number ≤ 20000; GPU always pads as 93-byte. |
| **P10** | medium | `app/src/opencl_dia.rs` | Diamond GPU success trusts medium hash; no `x16rs_hash` recompute + byte-compare (unlike block `verify_gpu_best_result`). |
| **P12** | medium | `app/src/diaworker.rs` | After `MAX_SUBMIT_ATTEMPTS` or soft parse failure, drained `DiamondMint` is logged and dropped — no durable save/requeue. |
| **P13** | medium | `x16rs/opencl/x16rs_diamond.cl` | Per-work-item reduction never seeds `best_name` from unit index 0 before `i=1..` comparisons. |
| **P14** | low | `app/src/block_mining_runtime.rs` | Drain aggregates planned `res.nonce_space` even when recovery only mined a partial window — telemetry overstated. |
| **P15** | low | `app/src/opencl_gpu/block.rs` | OpenCL accepts stuff length ≤ 512 without enforcing the 89-byte block intro CUDA requires. |

---

## 4. New bugs

| Severity | File | Issue | Impact |
|----------|------|-------|--------|
| **high** | `app/src/poworker.rs` | Notice long-poll only breaks when notice height ≥ `pending_height`; fullnode notice reports chain tip (typically pending−1), so timeouts and same-height tip reorgs never re-fetch pending. `intro_changed` path is effectively dead on fullnode. | After tip reorg, workers hash orphaned parent for up to a full block interval; PoW cannot land on main chain. |
| **high** | `app/src/poworker.rs` | `leave_upstream_stale()` runs as soon as `block_intro` is present, before `set_pending_block_stuff` succeeds and even when install is skipped by the height/intro gate. | After upstream-stale outage, workers can resume grinding a previous dead template if recovery install fails/skips. |
| **high** | `app/src/diaworker.rs` | `pull_and_push_diamond` only advances when `next_num > mining_num`; never rolls number backward or refreshes `prev_hash` / `born.hash` after diamond reorg. | Workers mine invalid `(number, prev_hash)` until chain mints past stale number; successes fail node validation. |
| **high** | `miner-pool/src/job.rs` | `JobHub::update` always sets `job_id=h{height}` only; stratum dedup keys solely on `job_id`, so same-height template changes never emit `mining.notify`. | Stratum miners stay on orphaned/obsolete template while HTTP path can serve new job; shares miss live tip. |
| **medium** | `app/src/mining_batch.rs` | OpenCL batch/integrity failures only trigger `on_batch_error` + bounded CPU recovery; no consecutive-failure budget or session GPU-disable (CUDA has both at 20). | Permanently failing OpenCL device never fail-closes; miner stuck on tiny CPU recovery, masking hardware death. |
| **medium** | `app/src/block_mining_runtime.rs` | `set_pending_block_stuff` does not require JSON height == `block_intro.height()`; mining uses JSON height for x16rs repeat while consensus uses intro-embedded height. | Mismatched upstream height → wrong-repeat mining and/or submit rejection. |
| **medium** | `app/src/block_mining_runtime.rs` | Result drain thread returns immediately on `stop_flag` without draining the result channel; only submit queue is wound down. | Clean shutdown/restart can discard target-meeting winners still in the result channel (lost shares/blocks). |
| **medium** | `x16rs/opencl/x16rs_diamond.cl` | Kernel reduction ranks solely by `diamond_more_power` (more leading zeros); consensus requires **exactly** `DMD_L` zeros — overshoots are invalid. | Valid 10-zero diamond lost when an invalid 11+ overshoot is in the same unit/WG. |
| **medium** | `app/src/opencl_dia.rs` | Diamond GPU post-process never bounds-checks nonces against `[nonce_start, nonce_start+nonce_space)` (block path does). | Corrupted/out-of-window nonces can pass partial checks → false success or wasted submits. |
| **medium** | `app/src/opencl_dia.rs` | On `check_diamer_success`, function returns before `needs_queue_finish` / `queue.finish()`; block OpenCL always finishes on RDNA4/duplicate ICD. | After diamond success, AMD queue may not drain → stale/out-of-order batches. |
| **low** | `x16rs/opencl/util.cl` | `block_t` is 88 bytes; GPU SHA3 hardcodes pad lane forcing `intro[88]==0`; CPU/consensus use full 89-byte intro (`witness_stage`). | Latent while `witness_stage` is zero; non-zero 89th byte makes GPU SHA3 diverge from consensus. |

---

## 5. Unclear

None. Prior rechecks produced no unclear outcomes.

---

## 6. Recommended next fix order

Priority groups by **payout / correctness risk**, then **cluster affinity** (fix one area together).

### Tier 0 — Template / reorg correctness (blocks payout path)

1. **New: notice long-poll / tip vs pending** (`poworker.rs` + fullnode `miner_notice`) — unlocks same-height reorg path; without this, **P2**/`intro_changed` barely matter on fullnode.
2. **New: `leave_upstream_stale` before install** (`poworker.rs`) — stop grinding dead templates after outage recovery.
3. **P2** — install on height decrease (reorg to lower pending).
4. **P6** — write `LAST_PENDING_INTRO` only after successful `set_pending_block_stuff`.
5. **P1 + P7 + P8** together — epoch/template id on `BlockMiningResult`; filter drain; stop workers on height decrease; revalidate before submit.
6. **New: height vs intro.height mismatch** (`set_pending_block_stuff`) — cheap invariant, prevents wrong-repeat mining.

### Tier 1 — Pool / diamond job identity

7. **New: stratum `job_id=h{height}` only** (`miner-pool/job.rs` + stratum notify) — include intro/content hash so same-height reorgs notify.
8. **New: diamond number/prev_hash never roll back** (`diaworker.rs` `pull_and_push_diamond`) — reorg-safe diamond job refresh.

### Tier 2 — OpenCL diamond correctness (find loss)

9. **P3 + P13** — seed reduction from unit index 0 / current work-item (match block kernel pattern).
10. **P4** — `CLK_LOCAL_MEM_FENCE | CLK_GLOBAL_MEM_FENCE` on diamond barriers.
11. **New: diamond_more_power overshoot** — prefer valid exact-`DMD_L` names over stronger invalid overshoots (or validate before reduce).
12. **P9** — 61-byte vs 93-byte diamond SHA3 padding for number ≤ 20000.
13. **P10 + New: nonce window + finish-on-success** (`opencl_dia.rs`) — recompute medium hash, bounds-check nonces, always `queue.finish` when required.

### Tier 3 — Submit durability & fail-close

14. **P5 + P12** — durable diamond submit requeue / save on network and soft parse failure.
15. **New: result channel abandon-on-stop** — final drain before result thread exit.
16. **New: OpenCL consecutive-failure GPU disable** — parity with CUDA session latch.

### Tier 4 — Low / latent

17. **P14** — report actual mined nonce space, not planned.
18. **P15** — enforce 89-byte block intro on OpenCL upload.
19. **New: block_t 88-byte / intro[88]** — load 89th byte into SHA3 when `witness_stage` can be non-zero.

---

### Cluster map (for parallel workstreams)

| Stream | Items |
|--------|--------|
| **A. Block job lifecycle** | Notice long-poll, leave_upstream_stale, P2, P6, P1, P7, P8, height==intro.height |
| **B. Pool / diamond jobs** | Stratum job_id, diamond number rollback |
| **C. Diamond GPU kernel** | P3, P4, P13, overshoot, P9 |
| **D. Diamond host post** | P10, nonce window, finish-on-success, P5, P12 |
| **E. Ops / telemetry** | OpenCL GPU disable, shutdown drain, P14, P15, block_t 89th byte |

---

### Severity rollup (open only)

| Severity | Still open | New | Total |
|----------|----------:|----:|------:|
| high | 5 | 4 | **9** |
| medium | 7 | 6 | **13** |
| low | 2 | 1 | **3** |
| **Total** | **14** | **11** | **25** |
