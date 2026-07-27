# CUDA end-to-end on Colab: node + payout pool + CUDA miner

Same scenario the AMD gfx1201 rig ran locally, so the two are directly
comparable. Each cell is standalone; a Colab `!` cell is a fresh shell, so the
environment is re-sourced every time.

Pick a GPU runtime first: Runtime -> Change runtime type -> T4 GPU.

What this proves, in order: the CUDA kernels are byte-correct, the CUDA share
list pays the card for every nonce it mines (Cell 6, the reason this document
was updated), a CUDA miner wins real blocks, and the full money path
(shares -> PPLNS -> maturity -> payout) runs on NVIDIA exactly as it does on AMD.

---

## Cell 1: clone and build

```python
!nvidia-smi --query-gpu=name,memory.total,driver_version --format=csv,noheader
!test -d /content/fullnodedev || git clone --depth 1 -b feat/pool-directory-cuda-ptx-panel https://github.com/Moskyera/fullnodedev.git /content/fullnodedev
!test -f "$HOME/.cargo/env" || (curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal)
!cd /content/fullnodedev && . "$HOME/.cargo/env" && export CUDA_PATH=/usr/local/cuda && export PATH=$PATH:/usr/local/cuda/bin && \
  cargo build --release --features cuda --bin fullnode --bin poworker 2>&1 | tail -5 && \
  cargo build --release -p hbit-pool --bin hbit-pool-server 2>&1 | tail -3
```

Expect `Finished release profile`. The first build takes roughly 7 to 8 minutes.

---

## Cell 2: correctness before throughput

A fast miner that computes the wrong hash is worth nothing, so pin the kernels
against the CPU implementation first.

```python
!cd /content/fullnodedev && . "$HOME/.cargo/env" && export CUDA_PATH=/usr/local/cuda && export PATH=$PATH:/usr/local/cuda/bin && \
  cargo test -p x16rs-cuda --release --features cuda 2>&1 | tail -30
```

`--features cuda` is not optional here. Every GPU test is gated behind it, so
without it the run silently passes with only the one CPU test and proves nothing.
If the output says `1 passed` you forgot the flag.

Two test binaries run, and BOTH matter:

- `tests/genesis_vector.rs`: 4 tests, including
  `cuda_matches_cpu_across_many_inputs` (4096 inputs at repeat 1, 512 at repeat
  16) and `cuda_batch_matches_cpu`. These are the byte-for-byte differential
  tests the whole product rests on.
- `src/lib.rs` unit tests, which on a machine with a real device also run
  `gpu_share_list_tests::the_share_list_matches_the_cpu_and_leaves_the_best_result_untouched`.
  That one is the share-list port: it checks that a SOLO batch returns exactly
  the CPU's single best result with an empty list, that the easiest possible
  target makes the counter see every nonce in the window while the list stores
  its capacity and reports the rest as overflow, that a strict target returns
  exactly the payable nonces and nothing else, and that a pool batch does not
  leak its counter into the next solo batch.

If that test prints `no usable CUDA device ... skipping`, the runtime has no GPU
attached. Fix the runtime type before going further; nothing below is meaningful.

If any differential test fails, stop. A share list that hands the pool wrong
hashes is worse than no share list.

---

## Cell 3: configs

`difficulty_adjust_blocks` is deliberately 8 rather than the 288 default. At
bootstrap difficulty the share target and the block target coincide, so the pool
can never return `kind: "share"` and the PPLNS accounting cannot be exercised at
all. Shrinking the window makes ASERT engage after 9 blocks, so the run reaches
real difficulty in minutes instead of hours.

The second worker config is the point of the whole exercise. On the AMD rig the
defect was invisible in the miner's own numbers: the card reported a healthy
hashrate while a single-threaded CPU miner beside it took the entire PPLNS
window. So this run puts that CPU rival back, on its own address, and Cell 6
measures the split.

```python
import pathlib
D = pathlib.Path("/content/fullnodedev/target/release")

GPU_WORKER = "1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS"
CPU_WORKER = "1AhGNNrHUNaiwS2GWBPR4UuDXjEiDwoE3v"

(D/"hacash.config.ini").write_text("""[node]
listen = 13337
not_find_nodes = true
fast_sync = false

[mint]
chain_id = 2
difficulty_adjust_blocks = 8
each_block_target_time = 10

[server]
enable = true
listen = 18080
bind = 127.0.0.1

[miner]
enable = true
reward = %s
message = hbit-colab
""" % GPU_WORKER)

(D/"poworker.config.ini").write_text("""connect = 127.0.0.1:18082
pool_worker = %s
supervene = 0
nonce_max = 4294967295
notice_wait = 3

[gpu]
use_opencl = false
use_cuda = true
cuda_device = 0
work_groups = 256
local_size = 256
unit_size = 16

[efficiency]
mode = speed
dynamic_supervene = false
oom_fallback = true
max_temp_c = 0
pause_if_unprofitable = false
stats_file = miner-stats.json
""" % GPU_WORKER)

# The rival: one CPU thread, no GPU, its own pool address and its own stats
# file.
#
# It gets its own directory only so its stats file does not collide with the
# GPU's. What actually selects its config is the command-line argument, and that
# matters more than it looks (sys/src/config.rs resolve_config_path_from):
#
#   if args.len() == 2 { PathBuf::from(&args[1]) } else { executable_dir.join(..) }
#
# So an argument IS honoured, and without one the default is resolved against the
# EXECUTABLE'S directory, not the working directory. current_exe() reads
# /proc/self/exe, which follows symlinks, so launching a symlinked copy of the
# binary from this directory lands back in the GPU's directory and loads the GPU
# config: two CUDA miners on one card, both paid to the same address, no rival at
# all. Hence an absolute config path, passed explicitly, and no symlink.
RIVAL = D / "cpurival"
RIVAL.mkdir(exist_ok=True)
(RIVAL/"poworker.config.ini").write_text("""connect = 127.0.0.1:18082
pool_worker = %s
supervene = 1
nonce_max = 4294967295
notice_wait = 3

[gpu]
use_opencl = false
use_cuda = false

[efficiency]
mode = speed
dynamic_supervene = false
oom_fallback = true
max_temp_c = 0
pause_if_unprofitable = false
stats_file = cpu-miner-stats.json
""" % CPU_WORKER)

print("configs written to", D)
print("GPU worker:", GPU_WORKER)
print("CPU worker:", CPU_WORKER)
```

---

## Cell 4: run node + pool + CUDA miner + CPU rival

Everything runs in ONE cell so the four processes genuinely overlap. A previous
attempt failed because the node had already exited before the miner started.

The final `/stats` snapshot is written to `/content/final_stats.json` before the
processes are torn down, because Cell 6 needs the PPLNS window split and the
pool is gone by then.

```python
import subprocess, os, time, json, urllib.request, re
D = "/content/fullnodedev/target/release"
GPU_WORKER = "1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS"
CPU_WORKER = "1AhGNNrHUNaiwS2GWBPR4UuDXjEiDwoE3v"
env = dict(os.environ, LD_LIBRARY_PATH="/usr/local/cuda/lib64:" + os.environ.get("LD_LIBRARY_PATH",""))
subprocess.run("pkill -9 fullnode; pkill -9 poworker; pkill -9 hbit-pool-server; sleep 3; rm -rf %s/hacash_*_data %s/pool-wallet.key*" % (D,D), shell=True)

def get(url, t=3):
    return json.loads(urllib.request.urlopen(url, timeout=t).read().decode())

node = subprocess.Popen(["./fullnode"], cwd=D, stdout=open("/content/node.log","w"),
                        stderr=subprocess.STDOUT, env=env, start_new_session=True)
for _ in range(60):
    try:
        print("NODE UP height =", get("http://127.0.0.1:18080/query/latest")["height"]); break
    except Exception: time.sleep(1)

# PHASE 1: raise the difficulty before the pool exists.
#
# At height 0 the chain sits at LOWEST_DIFFICULTY, where the share target
# saturates and every hash is a share. The pool REFUSES to start on that, by
# design, because credit would track submission rate instead of hashrate. So the
# GPU mines SOLO against the node first, until ASERT has pulled the difficulty
# off its floor. Cell 3 set difficulty_adjust_blocks = 8, so this takes a couple
# of minutes rather than the 289 blocks the default would need.
# Normalise to the pool address FIRST and keep that as the copy to restore.
# Snapshotting the file as found would be a trap: if this cell dies during the
# warm-up it leaves the config pointing at the node, and the next run would
# "restore" that, so phase 2 would mine solo while the pool sat at zero shares
# and nothing would report a problem.
CFG = D + "/poworker.config.ini"
pool_cfg = open(CFG).read().replace("connect = 127.0.0.1:18080", "connect = 127.0.0.1:18082")
open(CFG, "w").write(pool_cfg.replace("connect = 127.0.0.1:18082", "connect = 127.0.0.1:18080"))
warmup = subprocess.Popen(["./poworker"], cwd=D, stdout=open("/content/warmup.log","w"),
                          stderr=subprocess.STDOUT, env=env, start_new_session=True)
# Wait for the exact condition the pool checks, rather than a proxy for it.
#
# /query/miner/pending sends target_hash, not a difficulty number. With
# share_bits = 24 the served share target is the network target made 24 bits
# easier, but it cannot be made easier than the all-ones ceiling, so the factor a
# worker really gets is min(24, N) where N is the leading zero bits of the network
# target. The pool demands 18. So count N and wait for it, with a little margin
# because ASERT keeps moving.
#
# Counting is also safer than prefix-matching the bootstrap target. That target
# is not simply u32_to_hash(LOWEST_DIFFICULTY): the endpoint passes it through
# right_00_to_ff, which decrements the last non-zero byte and fills the tail with
# ff, so FF FF FE 00.. is published as fffffdffff... A prefix test has to get that
# transform right to mean anything, and a wrong digit fails open, exiting the
# warm-up while the chain is still on its floor.
def lzbits(hexstr):
    n = 0
    for ch in hexstr:
        v = int(ch, 16)
        if v:
            return n + 4 - v.bit_length()
        n += 4
    return n

NEED = 20
print("warming the chain up off LOWEST_DIFFICULTY (solo, no pool yet)...")
ready = False
for _ in range(40):
    time.sleep(15)
    h = get("http://127.0.0.1:18080/query/latest")["height"]
    t = get("http://127.0.0.1:18080/query/miner/pending")["target_hash"]
    n = lzbits(t)
    print("  height %-4d target %s  zero bits %-3d share factor %d" % (h, t[:16], n, min(24, n)))
    if n >= NEED:
        ready = True
        break
if not ready:
    print("WARNING: the target never reached %d zero bits, so a share would still cost" % NEED)
    print("almost nothing and the pool will refuse to start. It is right to. Give it longer.")
warmup.terminate(); time.sleep(3)
open(CFG, "w").write(pool_cfg)   # back to the pool address
assert "18082" in open(CFG).read(), "the worker config must point at the pool for phase 2"

# PHASE 2: now the pool can serve work that costs something.
pool = subprocess.Popen(["./hbit-pool-server","http://127.0.0.1:18080","pool-wallet.key",
                         "127.0.0.1:18082","24","testnet:8:10","120"], cwd=D,
                        stdout=open("/content/pool.log","w"), stderr=subprocess.STDOUT,
                        env=env, start_new_session=True)
time.sleep(10)
print(open("/content/pool.log").read()[-800:])
if pool.poll() is not None:
    raise SystemExit("the pool refused to start; read the message above. If it is the share "
                     "target saturating, the warm-up did not raise the difficulty enough.")

miner = subprocess.Popen(["./poworker"], cwd=D, stdout=open("/content/miner.log","w"),
                         stderr=subprocess.STDOUT, env=env, start_new_session=True)
# The rival takes its config as an ABSOLUTE argument. Its own directory is only
# for the stats file; it cannot select the config, because poworker resolves the
# default against the executable's directory and current_exe() follows symlinks.
RIVAL_CFG = D + "/cpurival/poworker.config.ini"
rival = subprocess.Popen([D + "/poworker", RIVAL_CFG], cwd=D + "/cpurival",
                         stdout=open("/content/cpu.log","w"), stderr=subprocess.STDOUT,
                         env=env, start_new_session=True)
time.sleep(6)
_riv = open("/content/cpu.log").read()
# poworker prints the canonical path of the file it loaded. That is the direct
# evidence, so check it rather than inferring from behaviour. It matters because
# an unreadable config is not fatal: load_config_path prints "[Config Error]" and
# hands back an EMPTY map, so the rival would run on defaults and look plausible.
if RIVAL_CFG not in _riv:
    print(_riv[:800])
    raise SystemExit("the rival did not load " + RIVAL_CFG + "; its log is above")
if re.search(r"Create CUDA block miner worker|\[CUDA\] Device #", _riv):
    raise SystemExit("the rival came up as a CUDA worker: it must be the CPU worker on its own "
                     "address, or the whole measurement is meaningless")
print("CUDA miner + single-thread CPU rival started, sampling for 10 minutes...")
stats = {}
for i in range(10):
    time.sleep(60)
    stats = get("http://127.0.0.1:18082/stats")
    h = get("http://127.0.0.1:18080/query/latest")["height"]
    counts = dict((a, n) for a, n in stats["workers"])
    g, c = counts.get(GPU_WORKER, 0), counts.get(CPU_WORKER, 0)
    pct = (100.0 * g / (g + c)) if (g + c) else 0.0
    print("t+%2dmin node_h=%-4d diff=%-12d shares=%-6d pend=%d orph=%d conf=%d | window gpu=%-6d cpu=%-5d gpu=%.1f%%"
          % (i+1, h, stats["difficulty"], stats["accepted_shares"], stats["blocks_pending"],
             stats["blocks_orphaned"], stats["blocks_confirmed"], g, c, pct))
json.dump(stats, open("/content/final_stats.json","w"))
for p in (rival, miner, pool, node): p.terminate()
```

Watch `diff` leave 4294967294: that is ASERT engaging and the point where shares
become distinct from blocks. Watch `gpu=%` too: it should be at or near 100 from
the first minute the difficulty is real. If it hovers near the CPU's share while
the GPU is orders of magnitude faster, the share list is not working and Cell 6
will say so.

---

## Cell 5: the raw counts

```python
import re
log = open("/content/miner.log").read()
def n(p): return len(re.findall(p, log))
rates = [float(x) for x in re.findall(r"([0-9.]+)MH/s", log)]
print("CUDA worker created :", "Create CUDA block miner worker" in log)
print("hashrate peak       : %.2f MH/s" % (max(rates) if rates else 0))
print("total submits       :", n(r"submit/miner/success"))
print("  kind=share        :", n(r'"kind":"share"'))
print("  kind=block        :", n(r'"kind":"block"'))
print("  kind=stale        :", n(r'"kind":"stale"'))
print("MINING SUCCESS      :", n(r"MINING SUCCESS"))
print("SUBMIT FAILED       :", n(r"MINING SUBMIT FAILED"))
print("\n--- template gate reports ---")
print("\n".join(re.findall(r"\[Mining\] height .*", log))[:1500])
print("\n--- pool settlement ---")
print("\n".join(l for l in open("/content/pool.log") if any(k in l for k in ("settle","reorg","payout"))))
```

### What a healthy run looks like

- `kind=share` should dominate once ASERT has engaged. If it is 0 while
  `kind=block` is high, the chain never left bootstrap difficulty and the PPLNS
  path was not exercised.
- `SUBMIT FAILED` should be 0 and `kind=stale` should be a small fraction. A
  large stale count means the submit gate is not settling dead templates.
- One `[Mining] height N settled` line per template, not thousands of rejections.
- `blocks_orphaned` above zero is normal and healthy: it means the pool noticed
  its own block losing a race and did not pay out on it.
- `holding back N unit(s) ... not yet buried 16 deep` is the coinbase maturity
  guard. Payouts lag block discovery by design.

An old note in this document quoted an AMD gfx1201 baseline of 205 MH/s and 333
total submits (281 share, 15 block, 37 stale, 0 failed). Do NOT use that as the
bar. A few hundred submits over a ten minute run is roughly one per template,
which is the exact defect the share list fixes: the card mined billions of hashes
and got credit for a handful of them. The bar is in Cell 6.

---

## Cell 6: the share list, and whether the card is paid for what it mines

This is the cell that tells you whether the CUDA port of the share list works.

The defect it targets: the batch kernel found thousands of payable nonces per
batch and the miner reported only the single minimum from the tree reduction.
Measured on the AMD gfx1201 against a live pool, before and after the OpenCL fix:

| measurement                                   | before  | after   |
| --------------------------------------------- | ------- | ------- |
| submissions in 8 minutes                      | 77      | 153,467 |
| share of the PPLNS window, testnet difficulty | 0.2%    | 64%     |
| share of the PPLNS window, REAL difficulty    |         | 99.8%   |

That last row is the one that matters, and it is the one this cell reproduces.
At real mainnet difficulty the fixed AMD card took 48 shares to the CPU's 1 in
the first minute and 431 to 1 by minute nine. The GPU is thousands of times
faster than one CPU thread, so it must take essentially the whole window. Taking
half of it, or a fifth of it, is the defect.

```python
import re, json

def rate_hps(text):
    """Peak hashrate from a poworker log, in hashes/sec. The log prints
    H/s, kH/s, MH/s or GH/s depending on the speed, and a single CPU thread
    never reaches MH/s, so a MH/s-only regex would read it as zero."""
    mult = {"": 1.0, "k": 1e3, "M": 1e6, "G": 1e9, "T": 1e12}
    best = 0.0
    for num, unit in re.findall(r"([0-9]+\.?[0-9]*)\s*([kMGT]?)H/s", text):
        try: best = max(best, float(num) * mult.get(unit, 1.0))
        except ValueError: pass
    return best

GPU_WORKER = "1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS"
CPU_WORKER = "1AhGNNrHUNaiwS2GWBPR4UuDXjEiDwoE3v"
SAMPLE_MINUTES = 10

gpu_log = open("/content/miner.log").read()
cpu_log = open("/content/cpu.log").read()
stats   = json.load(open("/content/final_stats.json"))
counts  = dict((a, n) for a, n in stats["workers"])

gpu_submits = len(re.findall(r"submit/miner/success", gpu_log))
cpu_submits = len(re.findall(r"submit/miner/success", cpu_log))
gpu_shares  = len(re.findall(r'"kind":"share"', gpu_log))
# One "req height N target ... to mining" line per template the miner installed.
templates   = len(re.findall(r"req height \d+ target", gpu_log))
undersample = len(re.findall(r"UNDERSAMPLING", gpu_log))
dropped     = re.findall(r"([0-9]+) payable nonces from this batch were never submitted", gpu_log)

gpu_hps, cpu_hps = rate_hps(gpu_log), rate_hps(cpu_log)
gpu_win, cpu_win = counts.get(GPU_WORKER, 0), counts.get(CPU_WORKER, 0)
window = gpu_win + cpu_win

actual   = (gpu_win / window) if window else 0.0
expected = (gpu_hps / (gpu_hps + cpu_hps)) if (gpu_hps + cpu_hps) else 0.0

print("=" * 72)
print("SUBMISSION VOLUME")
print("  CUDA submissions           : %d  (%.1f per second over %d min)"
      % (gpu_submits, gpu_submits / (SAMPLE_MINUTES * 60.0), SAMPLE_MINUTES))
print("    of which kind=share      : %d" % gpu_shares)
print("  templates the miner saw    : %d" % templates)
print("  submissions per template   : %.1f" % (gpu_submits / templates if templates else 0.0))
print("  CPU rival submissions      : %d" % cpu_submits)
print()
print("PPLNS WINDOW SPLIT (what the pool will actually pay on)")
print("  CUDA worker in window      : %d" % gpu_win)
print("  CPU rival in window        : %d" % cpu_win)
print("  CUDA share of the window   : %.2f%%" % (100.0 * actual))
print()
print("PROPORTIONALITY (the window must track hashrate, not batch cadence)")
print("  CUDA hashrate peak         : %15.1f H/s" % gpu_hps)
print("  CPU rival hashrate peak    : %15.1f H/s   (CUDA is %.0fx faster)"
      % (cpu_hps, (gpu_hps / cpu_hps) if cpu_hps else 0.0))
print("  window share it should get : %.3f%%" % (100.0 * expected))
print("  window share it did get    : %.3f%%" % (100.0 * actual))
print()
print("SHARE LIST CAPACITY")
print("  UNDERSAMPLING warnings     : %d" % undersample)
if dropped:
    print("  nonces dropped (last)      : %s" % dropped[-1])
print("=" * 72)

verdict = []
def check(ok, label, detail):
    verdict.append(ok)
    print("%-6s %-38s %s" % ("PASS" if ok else "FAIL", label, detail))

check(gpu_submits >= 1000, "submission volume",
      "%d submissions; the broken shape is one per template (double or low triple digits)" % gpu_submits)
check(templates == 0 or gpu_submits / max(templates, 1) >= 10.0, "submissions per template",
      "%.1f; one per template means the tree-reduction minimum is still all that is reported"
      % (gpu_submits / templates if templates else 0.0))
check(window > 0 and actual >= 0.90, "PPLNS window share",
      "%.2f%%; the fixed AMD card took 99.8%% at real difficulty" % (100.0 * actual))
check(expected == 0.0 or actual >= 0.90 * expected, "window tracks hashrate",
      "got %.3f%% of the window on %.3f%% of the hashrate" % (100.0 * actual, 100.0 * expected))

print()
if all(verdict):
    print("RESULT: PASS. The CUDA share list is reporting every payable nonce, and")
    print("the card is being paid in proportion to the work it did.")
else:
    print("RESULT: FAIL. Do not ship this. A failing window-share line with a")
    print("healthy hashrate is the original defect: the card mines and the pool")
    print("credits somebody else.")

if undersample:
    print()
    print("NOTE (not a failure): the share list filled up, so the kernel counted more")
    print("payable nonces than one batch can hand back. That is the fix WORKING and")
    print("saying so. It means this pool's share target is far too easy for this card;")
    print("raise share_bits. The count above is income the miner did not claim.")
```

### Reading the output

- **All four PASS.** The port is good: the kernel is appending every hit, the
  host is reading the live prefix, the CPU re-hash accepted every entry, and the
  pool credited the card in proportion to its hashrate.
- **Submission volume FAILs, hashrate looks fine.** This is the original defect
  exactly as it was on AMD. The kernel is still reporting only the reduction
  minimum: check that `share_capacity` reaches the kernel non-zero, i.e. that the
  miner really is pooled (`pool_worker` set, `connect` pointing at the pool).
- **Window share FAILs while submission volume PASSes.** The submissions are
  being made but not credited. Look at `kind=stale` in Cell 5 and at the pool log:
  this is a submit-gate or template-freshness problem, not a kernel problem.
- **Everything PASSes but `UNDERSAMPLING` is non-zero.** The fix works and the
  pool is misconfigured for this card. Raise `share_bits` (the `24` argument in
  Cell 4) until the warning stops.
- **A `GPU integrity error` in the miner log.** The CPU could not reproduce a
  hash the card reported, or the card listed a nonce whose hash does not actually
  beat the share target. Every share list entry is re-hashed on the CPU before it
  can reach the pool, and one bad entry fails the whole batch by design. This is a
  hardware or kernel fault, never something to work around.

### Why the first minute is slower than the rest

The miner's submit gate normally allows one winner per template, because a
height holds exactly one block and a second winner for it is worthless. It drops
that rule the moment the upstream answers `kind: "share"` for the first time,
because a pool credits every share against the same template and suppressing
them would be the miner refusing its own income. So the submission rate steps up
once the first share is credited. A run that never leaves bootstrap difficulty
never gets that first `kind: "share"`, which is why Cell 3 shrinks
`difficulty_adjust_blocks` to 8.

### Why the absolute numbers differ from AMD

The 153,467 figure came from a gfx1201 at 205 MH/s. A T4 is far slower, so its
absolute submission count will be lower. The window-share lines are the
hardware-independent test and are the ones to trust: they compare this card
against a CPU thread on the same box, in the same PPLNS window, which is exactly
how the defect was found in the first place.

---

## About the payout line

Templates do carry mempool transactions, and a block submitted through the miner
API keeps them. That was checked in the code and is not the reason an earlier run
saw a payout go nowhere: `/submit/transaction` used to answer ret 0 before the
node had accepted anything, so the pool was told a transaction had landed when it
had not. Both halves are fixed, so the settlement lines here can be trusted:
"the node holds it" means the node really holds it.

What remains true, and is a design property rather than a bug: the pool mines
coinbase-only blocks, so its payout transaction confirms only when some OTHER
miner includes it. On this isolated rig the pool is the only miner, so a payout
can sit in the mempool indefinitely. If worker balances stay at `0:0` while the
pool keeps logging a pending payout, that is this, not CUDA. After three skipped
cycles the pool now says so itself in a warning instead of going quiet.

To watch a payout actually confirm, add a second block producer: leave
`[miner] enable = true` on the node (it is, in Cell 3) and let the node's own
CPU miner pack the mempool while the CUDA miner races it.
