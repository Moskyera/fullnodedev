# CUDA end-to-end on Colab: mainnet node + payout pool + CUDA miner

Pick a GPU runtime first: Runtime -> Change runtime type -> T4 GPU.

What this proves, in order: the CUDA kernels are byte-correct against the CPU
(Cell 2), and the CUDA share list gets the card credited in proportion to the work
it does, measured against a single CPU thread in the same PPLNS window at real
mainnet difficulty (Cells 3 to 7).

## Why this runs against mainnet, and not a local testnet

Earlier versions of this document mined a fresh local chain with
`difficulty_adjust_blocks = 8`, on the theory that shrinking the window would let
ASERT pull the difficulty up to something realistic within minutes. That is false,
and it invalidated every run built on it:

- Off mainnet the ASERT anchor is height `difficulty_adjust_blocks + 2`, and the
  target at that height is the fixed constant `ASERT_START_TARGET_NUM =
  0xe9cfffff` (`mint/src/check/difficulty_asert.rs`). `u32_to_hash` gives it
  `255 - 0xe9 = 22` leading zero bits. The chain does not climb to that value, it
  is pinned there.
- After the anchor, ASERT's half-life is 10800 seconds of WALL CLOCK, not of
  block-time budget. Blocks cannot arrive faster than one per second, because
  `block_build.rs` sets `nextts = max(now, prev_ts + 1)` and a release node
  rejects `blk_time <= prev_blk_time` (`chain/src/verify.rs`). At a 10 second
  target that is 1200 blocks, so 20 minutes, per bit of difficulty.
- So the chain sits at 22 leading zero bits for the whole run. With the pool's
  lowest legal `share_bits` of 18, the most a share could ever cost there is
  `2^4`, sixteen hashes. Every hash is effectively a share, PPLNS credit measures
  how fast a worker completes an HTTP round trip, and the proportionality figure
  the run exists to produce means nothing.

The pool now refuses to start in that regime rather than serving it, so a local
testnet cannot be used for this measurement at all. Real difficulty is the only
place the question can be asked, which is also where the AMD gfx1201 baseline in
Cell 7 was taken.

Nothing here spends money. The node syncs and validates public blocks, the pool
holds a throwaway wallet, and at mainnet difficulty nothing attached to it is
going to win a block.

---

## Cell 1: clone, update, build

```python
!nvidia-smi --query-gpu=name,memory.total,driver_version --format=csv,noheader
!set -e; test -d /content/fullnodedev || git clone --depth 1 -b feat/pool-directory-cuda-ptx-panel https://github.com/Moskyera/fullnodedev.git /content/fullnodedev
!set -e; cd /content/fullnodedev && git fetch --depth 1 origin feat/pool-directory-cuda-ptx-panel && git reset --hard FETCH_HEAD && git log --oneline -1
!test -f "$HOME/.cargo/env" || (curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal)
!set -eo pipefail; cd /content/fullnodedev && . "$HOME/.cargo/env" && export CUDA_PATH=/usr/local/cuda && export PATH=$PATH:/usr/local/cuda/bin && \
  cargo build --release --features cuda --bin fullnode --bin poworker 2>&1 | tail -5 && \
  cargo build --release -p hbit-pool --bin hbit-pool-server 2>&1 | tail -3
```

Expect `Finished release profile`. The first build takes roughly 7 to 8 minutes.

`set -eo pipefail` is load bearing. Without it a shell pipeline exits with the
status of `tail`, which is always 0, so a compile error scrolls past and the cell
reports success. Cargo leaves the previous binary in place when a build fails, so
the run would then measure whatever was built last time, which may predate the
share-list fix this document exists to test.

The `git fetch`/`reset --hard` matters for the same reason: `git clone` is skipped
when the directory already exists, so without it a Colab session that survived a
runtime restart silently re-tests an old commit.

---

## Cell 2: correctness before throughput

A fast miner that computes the wrong hash is worth nothing, so pin the kernels
against the CPU implementation first.

```python
!cd /content/fullnodedev && . "$HOME/.cargo/env" && export CUDA_PATH=/usr/local/cuda && export PATH=$PATH:/usr/local/cuda/bin && \
  cargo test -p x16rs-cuda --release --features cuda 2>&1 | tail -30
```

`--features cuda` is not optional. Every GPU test is gated behind it, so without
it the run passes with only the one CPU test and proves nothing. If the output
says `1 passed` you forgot the flag.

Two test binaries run, and BOTH matter:

- `tests/genesis_vector.rs`: 4 tests, including
  `cuda_matches_cpu_across_many_inputs` (4096 inputs at repeat 1, 512 at repeat
  16) and `cuda_batch_matches_cpu`. These are the byte-for-byte differential
  tests the whole product rests on.
- `src/lib.rs` unit tests, which on a machine with a real device also run
  `gpu_share_list_tests::the_share_list_matches_the_cpu_and_leaves_the_best_result_untouched`.
  That one is the share-list port itself: a SOLO batch returns exactly the CPU's
  single best result with an empty list; the easiest possible target makes the
  counter see every nonce in the window while the list stores its capacity and
  reports the rest as overflow; a strict target returns exactly the payable nonces
  and nothing else; and a pool batch does not leak its counter into the next solo
  batch.

If that test prints `no usable CUDA device ... skipping`, the runtime has no GPU
attached. Fix the runtime type before going further; nothing below is meaningful.

If any differential test fails, stop. A share list that hands the pool wrong
hashes is worse than no share list.

---

## Cell 3: configs

The node is a plain mainnet node with mining ENABLED. That flag does not make the
node hash anything: its only effects are to gate the two miner API routes
(`/query/miner/pending` and `/submit/miner/success`) and to size the transaction
pool. The pool needs those routes to fetch templates, which is the whole reason it
is on.

The second worker config is the point of the exercise. On the AMD rig the defect
was invisible in the miner's own numbers: the card reported a healthy hashrate
while a single-threaded CPU miner beside it took the entire PPLNS window. So this
run puts that CPU rival back, on its own address, and Cell 7 measures the split.

```python
import pathlib, os
D = pathlib.Path("/content/fullnodedev/target/release")

GPU_WORKER = "1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS"
CPU_WORKER = "1AhGNNrHUNaiwS2GWBPR4UuDXjEiDwoE3v"

# No [mint] section: chain_id defaults to 0, which IS mainnet. That is the point.
(D/"hacash.config.ini").write_text("""[node]
listen = 13337
boots = 54.193.49.59:3337, 182.92.163.225:3337, 54.219.80.127:3337
not_find_nodes = false
fast_sync = false

[server]
enable = true
listen = 18080
bind = 127.0.0.1

[miner]
enable = true
reward = %s
message = hbit-colab
""" % GPU_WORKER)

# 18082 is the pool. Nothing rewrites this file later: there is no warm-up phase
# to point it at the node, because the difficulty comes from the real chain.
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

# The rival: one CPU thread, no GPU, its own pool address and its own stats file.
#
# It gets its own directory only so its stats file does not collide with the
# GPU's. What SELECTS its config is the command-line argument
# (sys/src/config.rs resolve_config_path_from):
#
#   if args.len() == 2 { PathBuf::from(&args[1]) } else { executable_dir.join(..) }
#
# An argument IS honoured, and without one the default resolves against the
# EXECUTABLE'S directory, not the working directory. current_exe() reads
# /proc/self/exe, which follows symlinks, so launching a symlinked copy of the
# binary from this directory lands back in the GPU's directory and loads the GPU
# config: two CUDA miners on one card, both paid to the same address, no rival at
# all. Hence an absolute config path, passed explicitly, and no symlink.
RIVAL = D / "cpurival"
RIVAL.mkdir(exist_ok=True)
for stale in ("poworker", "opencl"):
    q = RIVAL / stale
    if q.is_symlink():
        os.unlink(q)          # left behind by an earlier version of this cell
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

## Cell 4: sync the node to the mainnet tip

This is the long cell. It downloads and validates the real chain, which is the
whole reason the measurement below means anything. Leave it running; it reports
progress every 30 seconds and stops on its own.

It is safe to re-run: the chain data persists under
`/content/fullnodedev/target/release/hacash_mainnet_data`, so a second run
resumes rather than starting over.

Expect about 2.7 GB of chain data, measured from a completed sync of the same
chain, which is comfortably inside Colab's disk. The time is dominated by
download and validation rather than by anything on the GPU.

```python
import subprocess, os, time, json, urllib.request
D = "/content/fullnodedev/target/release"
env = dict(os.environ, LD_LIBRARY_PATH="/usr/local/cuda/lib64:" + os.environ.get("LD_LIBRARY_PATH",""))

# Kill by EXACT process name, and include the 15-character truncation.
#
# The kernel stores comm in TASK_COMM_LEN bytes, 16 including the NUL, so a
# 16-character binary name is only ever visible as 15. "hbit-pool-server" is
# exactly 16, so `pkill hbit-pool-server` matches nothing, every time, and a pool
# left over from an earlier attempt keeps holding 18082. "fullnode" and
# "poworker" are 8 characters, which is why only the pool ever leaked.
#
# -x (exact match on comm) rather than -f (full command line) is deliberate: -f
# would match the shell running this very command, whose command line contains
# all three names, so the first pkill would kill the shell and the rest would
# never run.
KILL = ("pkill -9 -x fullnode; pkill -9 -x poworker; "
        "pkill -9 -x hbit-pool-server; pkill -9 -x hbit-pool-serve")
subprocess.run(KILL + "; sleep 3", shell=True)

def get(url, t=15, tries=3):
    """Retry. A single dropped connection must not end a run that costs an hour."""
    last = None
    for _ in range(tries):
        try:
            return json.loads(urllib.request.urlopen(url, timeout=t).read().decode())
        except Exception as e:
            last = e
            time.sleep(2)
    raise last

def lzbits(hexstr):
    n = 0
    for ch in hexstr:
        v = int(ch, 16)
        if v:
            return n + 4 - v.bit_length()
        n += 4
    return n

node = subprocess.Popen(["./fullnode"], cwd=D, stdout=open("/content/node.log","w"),
                        stderr=subprocess.STDOUT, env=env, start_new_session=True)
up = False
for _ in range(90):
    try:
        get("http://127.0.0.1:18080/query/latest", t=3, tries=1)
        up = True
        break
    except Exception:
        time.sleep(2)
if not up:
    print(open("/content/node.log").read()[-2000:])
    node.terminate()
    raise SystemExit("the node never answered on 18080; its log is above")

# Two conditions, both required.
#
# (1) The difficulty must be real. /query/miner/pending returns target_hash, and
#     its leading zero bits ARE the work a block costs as a power of two. Early
#     mainnet blocks are easy, so a node part way through the chain reports a low
#     figure; this is the direct measure of whether we have arrived somewhere the
#     question can be asked.
# (2) The tip must be stable. During fast_sync the height climbs by thousands per
#     minute; at the real tip mainnet produces roughly one block per 300 seconds.
#     Fewer than 5 blocks in 60 seconds means we are following, not catching up.
#     34 is not a guess at what mainnet costs, it is the lowest value at which
#     this measurement is possible at all: share_bits cannot go below 18, and the
#     pool requires a share to cost at least 2^16, so 18 + 16 is the floor. The
#     real tip is far above it. Setting this to an estimate of mainnet difficulty
#     instead would make the cell fail on a two hour timeout if the estimate were
#     high, which is the worse way to be wrong.
NEED_BITS = 34
prev_h, synced = None, False
print("syncing the real chain. This is the slow part; leave it running.")
for i in range(240):                      # 240 x 30s = up to two hours
    time.sleep(30)
    try:
        h = get("http://127.0.0.1:18080/query/latest")["height"]
        t = get("http://127.0.0.1:18080/query/miner/pending").get("target_hash")
    except Exception as e:
        print("  poll failed (%s); the node is busy, continuing" % e)
        continue
    if t is None:
        print("  height %-8d (no template yet)" % h)
        continue
    n = lzbits(t)
    rate = "" if prev_h is None else "  +%d blocks/30s" % (h - prev_h)
    print("  height %-8d work 2^%-3d%s" % (h, n, rate))
    if n >= NEED_BITS and prev_h is not None and (h - prev_h) < 5:
        synced = True
        break
    prev_h = h
if not synced:
    print(open("/content/node.log").read()[-1500:])
    node.terminate()
    raise SystemExit("the node did not reach a stable tip at 2^%d work within two hours. "
                     "Its log is above; check connectivity to the boot nodes." % NEED_BITS)

tip = get("http://127.0.0.1:18080/query/latest")["height"]
work = lzbits(get("http://127.0.0.1:18080/query/miner/pending")["target_hash"])
json.dump({"tip": tip, "work_bits": work}, open("/content/sync.json","w"))
print()
print("SYNCED. tip height %d, a block costs 2^%d hashes." % (tip, work))
print("Leave this node running and go straight to Cell 5.")
```

---

## Cell 5: run the pool, the CUDA miner and the CPU rival

The node from Cell 4 must still be running. This cell does not start one.

`share_bits` is computed, not typed. The pool serves a share target eased from the
network target by `share_bits`, so what a share COSTS is
`work_bits - share_bits`. Too easy and credit measures HTTP round trips rather
than hashing; too hard and the sample sees almost no shares. Twenty bits leaves a
share costing about a million hashes: tens per second for the card, well under one
per second for a single CPU thread, which is exactly the spread being measured.

```python
import subprocess, os, time, json, urllib.request, re
D = "/content/fullnodedev/target/release"
GPU_WORKER = "1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS"
CPU_WORKER = "1AhGNNrHUNaiwS2GWBPR4UuDXjEiDwoE3v"
env = dict(os.environ, LD_LIBRARY_PATH="/usr/local/cuda/lib64:" + os.environ.get("LD_LIBRARY_PATH",""))
RUN_ID = time.strftime("%Y%m%dT%H%M%S")

def get(url, t=15, tries=3):
    last = None
    for _ in range(tries):
        try:
            return json.loads(urllib.request.urlopen(url, timeout=t).read().decode())
        except Exception as e:
            last = e
            time.sleep(2)
    raise last

# Delete last run's evidence BEFORE producing this run's. Cells 6 and 7 read these
# files unconditionally, so an abort anywhere below would otherwise leave them
# reading the previous attempt byte for byte, and reprinting its verdict as if it
# belonged to this run.
subprocess.run("pkill -9 -x poworker; pkill -9 -x hbit-pool-server; "
               "pkill -9 -x hbit-pool-serve; sleep 2; "
               "rm -f /content/pool.log /content/miner.log /content/cpu.log "
               "/content/final_stats.json; rm -f %s/pool-wallet.key*" % D, shell=True)

sync = json.load(open("/content/sync.json"))
WORK_BITS = sync["work_bits"]
SHARE_COST_BITS = 20
SHARE_BITS = min(40, max(18, WORK_BITS - SHARE_COST_BITS))
print("run %s: a block costs 2^%d, share_bits=%d, so a share costs about 2^%d hashes."
      % (RUN_ID, WORK_BITS, SHARE_BITS, WORK_BITS - SHARE_BITS))
if WORK_BITS - SHARE_BITS < 16:
    raise SystemExit("the chain is too easy for an honest share here; the pool would refuse "
                     "and it would be right to. Re-run Cell 4 until the tip is real.")

procs = []
def spawn(argv, cwd, log):
    p = subprocess.Popen(argv, cwd=cwd, stdout=open(log, "w"),
                         stderr=subprocess.STDOUT, env=env, start_new_session=True)
    procs.append(p)
    return p

SAMPLE_MINUTES = 10
stats = {}
try:
    pool = spawn(["./hbit-pool-server", "http://127.0.0.1:18080", "pool-wallet.key",
                  "127.0.0.1:18082", str(SHARE_BITS), "mainnet", "120"], D, "/content/pool.log")
    time.sleep(12)
    poollog = open("/content/pool.log").read()
    print(poollog)                     # ALL of it: the startup summary states the
                                       # share factor actually served, and a tail
                                       # window can start after it.
    if pool.poll() is not None:
        raise SystemExit("the pool exited during startup; its message is above")
    if "caps it at" in poollog:
        raise SystemExit("the pool capped the share factor below what was asked, so the served "
                         "share target is at its ceiling and the split would be meaningless")

    miner = spawn(["./poworker"], D, "/content/miner.log")
    RIVAL_CFG = D + "/cpurival/poworker.config.ini"
    rival = spawn([D + "/poworker", RIVAL_CFG], D + "/cpurival", "/content/cpu.log")
    time.sleep(20)

    # Prove each miner is the one it claims to be, from what it printed rather
    # than from what was configured. poworker logs the canonical path of the
    # config it loaded, and a config it cannot read is NOT fatal:
    # load_config_path prints "[Config Error]" and returns an empty map, so a
    # mis-pathed rival would run on defaults and look entirely plausible.
    riv = open("/content/cpu.log").read()
    gpu = open("/content/miner.log").read()
    if RIVAL_CFG not in riv:
        print(riv[:1200])
        raise SystemExit("the rival did not load " + RIVAL_CFG + "; its log is above")
    if re.search(r"Create CUDA block miner worker|\[CUDA\] Device #", riv):
        raise SystemExit("the rival came up as a CUDA worker; it must be the CPU control")
    if "Create CUDA block miner worker" not in gpu:
        print(gpu[:1200])
        raise SystemExit("the GPU miner never created a CUDA worker; its log is above")

    print("CUDA miner and single-thread CPU rival running. Sampling for %d minutes..."
          % SAMPLE_MINUTES)
    samples = []
    for i in range(SAMPLE_MINUTES):
        time.sleep(60)
        # A dead miner must end the run, not produce ten minutes of zeroes. The
        # rival dying is the failure mode that reads as a PERFECT score.
        if miner.poll() is not None:
            raise SystemExit("the CUDA miner exited at minute %d" % (i + 1))
        if rival.poll() is not None:
            raise SystemExit("the CPU rival exited at minute %d, so there is no control" % (i + 1))
        try:
            stats = get("http://127.0.0.1:18082/stats")
        except Exception as e:
            print("t+%2dmin sample skipped (%s)" % (i + 1, e))
            continue
        counts = dict((a, n) for a, n in stats["workers"])
        g, c = counts.get(GPU_WORKER, 0), counts.get(CPU_WORKER, 0)
        pct = (100.0 * g / (g + c)) if (g + c) else 0.0
        samples.append({"minute": i + 1, "gpu": g, "cpu": c,
                        "window": stats.get("share_window"),
                        "accepted": stats["accepted_shares"]})
        print("t+%2dmin shares=%-7d window=%-5s | gpu=%-6d cpu=%-5d gpu=%.2f%%"
              % (i + 1, stats["accepted_shares"], stats.get("share_window"), g, c, pct))
        # Written EVERY minute, so a failure at minute nine still leaves a usable
        # snapshot instead of nothing.
        stats["run_id"] = RUN_ID
        stats["samples"] = samples
        stats["share_bits"] = SHARE_BITS
        stats["work_bits"] = WORK_BITS
        json.dump(stats, open("/content/final_stats.json", "w"))
finally:
    for p in reversed(procs):
        try:
            p.terminate()
        except Exception:
            pass
    time.sleep(2)
    subprocess.run("pkill -9 -x poworker; pkill -9 -x hbit-pool-server; "
                   "pkill -9 -x hbit-pool-serve", shell=True)
    open("/content/run_id.txt", "w").write(RUN_ID)
    print("run", RUN_ID, "finished; the node is still up for another run.")
```

---

## Cell 6: the raw counts

```python
import re
log = open("/content/miner.log").read()
cpu = open("/content/cpu.log").read()
def n(p, s=log): return len(re.findall(p, s))
print("run id              :", open("/content/run_id.txt").read().strip())
print("miner version line  :", (re.findall(r"\[Version\].*", log) or ["MISSING"])[0])
print("CUDA worker created :", "Create CUDA block miner worker" in log)
print("total submits       :", n(r"submit/miner/success"))
print("  kind=share        :", n(r'"kind":"share"'))
print("  kind=block        :", n(r'"kind":"block"'))
print("  kind=stale        :", n(r'"kind":"stale"'))
print("  kind=invalid      :", n(r'"kind":"invalid"'))
print("CPU rival submits   :", n(r"submit/miner/success", cpu))
# MINING SUBMIT FAILED is the catch-all arm: every stale, every invalid and every
# no-verdict submission prints it. On a pooled run stale is normal, so the raw
# count is expected to be large. What must be zero is the residual.
fail = n(r"MINING SUBMIT FAILED")
print("SUBMIT FAILED       :", fail)
print("  unexplained       :", fail - n(r'"kind":"stale"') - n(r'"kind":"invalid"'))
print("\n--- pool settlement ---")
print("\n".join(l for l in open("/content/pool.log")
                if any(k in l for k in ("settle", "reorg", "payout", "share factor"))))
print("\n--- template gate reports (EMPTY is the healthy pooled result) ---")
print("\n".join(re.findall(r"\[Mining\] height .*", log))[:1500])
```

### What a healthy run looks like

- `kind=share` dominates. `kind=block` will be 0: at mainnet difficulty nothing
  here wins a block, and that is expected. The share list is what is under test.
- `unexplained` must be 0. The raw `SUBMIT FAILED` count tracks `kind=stale`,
  because stale, invalid and no-verdict submissions all land in the same catch-all
  arm and print the same banner. A large stale count on a pooled run is ordinary:
  the pool rolls templates and in-flight submissions miss.
- The template gate section being EMPTY is the strongest single sign the share
  list is working. The gate suppresses redundant winners per template, and stops
  the moment the pool credits its first share, because each further share is
  separately payable work. Any `[Mining] height ... suppressed N` line means
  shares were dropped locally, which is money the miner earned and did not get.

---

## Cell 7: the share list, and whether the card is paid for what it mines

The defect this targets: the batch kernel found thousands of payable nonces per
batch and the miner reported only the single minimum from the tree reduction.
Measured on an AMD gfx1201 against a live pool, before and after the OpenCL fix:

| measurement                                   | before  | after   |
| --------------------------------------------- | ------- | ------- |
| submissions in 8 minutes                      | 77      | 153,467 |
| share of the PPLNS window, testnet difficulty | 0.2%    | 64%     |
| share of the PPLNS window, REAL difficulty    |         | 99.8%   |

The last row is the one that matters and the one this cell reproduces. The GPU is
far faster than one CPU thread, so it must take essentially the whole window.
Taking half of it, or a fifth, is the defect.

### Result on a Colab T4, 2026-07-27

Run `20260727T154114`, against the real chain at a block cost of 2^42, with
share_bits 22 leaving each share at about 2^20 hashes:

| | CUDA T4 | one CPU thread |
| --- | --- | --- |
| hashrate | 6.92 MH/s | 21.01 KH/s |
| submissions in 10 min | 2,493 | 6 |
| share of the PPLNS window | 99.76% | 0.24% |
| share it should have taken | 99.697% | |

The window share and the hashrate share agree to 0.06 of a percentage point, and
the split measured over the whole sample matches the window snapshot exactly. All
five checks passed, with no stale submissions, no failed submissions and no share
list overflow.

Two things about those numbers are worth stating so they are not misread later.

The 6.92 MH/s is not a regression against the 86 MH/s seen on the same card in
earlier runs. `block_hash_repeat` is `height / 50000 + 1`, capped at 16, so
mainnet heights hash with repeat 16 while a fresh chain at genesis uses repeat 1.
Sixteen times the work per hash is the entire difference, and this figure is the
mainnet-representative one.

The control produced only 6 shares, so the Poisson noise on it is roughly plus or
minus 2.4, which puts the measurable window share somewhere between 99.64% and
99.84%. The expected value sits inside that, so the agreement is real, but its
PRECISION is bounded by that count rather than by the GPU's. A longer sample, or a
rival with more threads, tightens it. Two other things this particular run did not
exercise: only one height change was observed, so template rolling and the stale
path were barely touched, and the share list never overflowed, so the
undersampling path was not reached outside the Cell 2 unit test that covers it
directly.

```python
import re, json, os, time

def rate_hps(text):
    """Peak hashrate from a poworker log, in hashes/sec.

    The unit prefix is UPPERCASE and has no separating space: rates_to_show uses
    HNS = ["K","M","G","T","P","E","Z","Y","B"] and formats "{:.2}{}H/s", so a
    kilohash reads "938.14KH/s". A pattern accepting only lowercase k cannot
    anchor on that at all and returns no match, which is not a small reading but
    a zero. A single x16rs CPU thread lives in exactly that band, so getting this
    wrong silently erases the control."""
    mult = {"": 1.0, "K": 1e3, "M": 1e6, "G": 1e9, "T": 1e12,
            "P": 1e15, "E": 1e18, "Z": 1e21, "Y": 1e24, "B": 1e27}
    best = 0.0
    for num, unit in re.findall(r"([0-9]+\.?[0-9]*)\s*([KMGTPEZYB]?)H/s", text):
        try:
            best = max(best, float(num) * mult[unit])
        except (ValueError, KeyError):
            pass
    return best

GPU_WORKER = "1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS"
CPU_WORKER = "1AhGNNrHUNaiwS2GWBPR4UuDXjEiDwoE3v"
SAMPLE_MINUTES = 10

gpu_log = open("/content/miner.log").read()
cpu_log = open("/content/cpu.log").read()
stats   = json.load(open("/content/final_stats.json"))

# These files are read unconditionally, so prove they belong to the same run
# before believing a single number in them.
run_id = open("/content/run_id.txt").read().strip()
assert stats.get("run_id") == run_id, \
    "final_stats.json is from run %s, not %s: Cell 5 aborted before finishing" % (
        stats.get("run_id"), run_id)
assert time.time() - os.path.getmtime("/content/final_stats.json") < 3600, \
    "these results are over an hour old; re-run Cell 5"

counts  = dict((a, n) for a, n in stats["workers"])
gpu_submits = len(re.findall(r"submit/miner/success", gpu_log))
cpu_submits = len(re.findall(r"submit/miner/success", cpu_log))
gpu_shares  = len(re.findall(r'"kind":"share"', gpu_log))
# Distinct heights, not raw lines. The line is printed by a drain tick whose
# results lag the installed template, and there is no per-template dedupe, so
# counting lines inflates the divisor exactly when the share list works best.
templates   = len(set(re.findall(r"req height (\d+) target", gpu_log)))
undersample = len(re.findall(r"UNDERSAMPLING", gpu_log))
# The warning carries TWO numbers: this batch, and the session total in the same
# line. The session total is the one that means anything.
dropped = re.findall(r"([0-9]+) payable nonces from this batch were never submitted "
                     r"\(([0-9]+) so far this session\)", gpu_log)

gpu_hps, cpu_hps = rate_hps(gpu_log), rate_hps(cpu_log)
gpu_win, cpu_win = counts.get(GPU_WORKER, 0), counts.get(CPU_WORKER, 0)
window = gpu_win + cpu_win
served_window = stats.get("share_window", window)

actual   = (gpu_win / window) if window else 0.0
expected = (gpu_hps / (gpu_hps + cpu_hps)) if (gpu_hps + cpu_hps) else 0.0
# The window is a rolling 4096 shares, so at these rates it turns over in under a
# minute and its split is a snapshot. Submissions over the whole sample are the
# same ratio measured over ten minutes, and the two should agree.
by_submits = (gpu_submits / (gpu_submits + cpu_submits)) if (gpu_submits + cpu_submits) else 0.0

print("=" * 72)
print("RUN %s: block costs 2^%d, share_bits=%d, share costs about 2^%d"
      % (run_id, stats["work_bits"], stats["share_bits"],
         stats["work_bits"] - stats["share_bits"]))
print()
print("SUBMISSION VOLUME")
print("  CUDA submissions           : %d  (%.1f per second over %d min)"
      % (gpu_submits, gpu_submits / (SAMPLE_MINUTES * 60.0), SAMPLE_MINUTES))
print("    of which kind=share      : %d" % gpu_shares)
print("  distinct heights mined     : %d" % templates)
print("  submissions per height     : %.1f" % (gpu_submits / templates if templates else 0.0))
print("  CPU rival submissions      : %d" % cpu_submits)
print()
print("PPLNS WINDOW SPLIT (what the pool will actually pay on)")
print("  CUDA worker in window      : %d" % gpu_win)
print("  CPU rival in window        : %d" % cpu_win)
print("  pool's own window total    : %s" % served_window)
print("  CUDA share of the window   : %.2f%%" % (100.0 * actual))
print("  CUDA share of submissions  : %.2f%%   (same ratio, whole sample)"
      % (100.0 * by_submits))
print()
print("PROPORTIONALITY (the window must track hashrate, not batch cadence)")
print("  CUDA hashrate peak         : %15.1f H/s" % gpu_hps)
print("  CPU rival hashrate peak    : %15.1f H/s   (CUDA is %.0fx faster)"
      % (cpu_hps, (gpu_hps / cpu_hps) if cpu_hps else 0.0))
print("  window share it should get : %.3f%%" % (100.0 * expected))
print("  window share it did get    : %.3f%%" % (100.0 * actual))
print()
print("SHARE LIST CAPACITY")
print("  UNDERSAMPLING reports      : %d   (throttled to one per 30s, so a lower bound)"
      % undersample)
if dropped:
    print("  nonces never claimed       : %s this batch, %s this session"
          % (dropped[-1][0], dropped[-1][1]))
print("=" * 72)

verdict = []
def check(ok, label, detail):
    verdict.append(bool(ok))
    print("%-6s %-38s %s" % ("PASS" if ok else "FAIL", label, detail))

# Every check requires its own measurement to exist. A guard of the form
# "<input missing> or <real test>" reports PASS for a run that recorded nothing,
# which is worse than a FAIL because it looks like an answer.
check(gpu_submits >= 1000, "submission volume",
      "%d submissions; the broken shape is one per template, in the low hundreds" % gpu_submits)
check(templates > 0 and gpu_submits / max(templates, 1) >= 10.0, "submissions per height",
      ("no heights parsed from the log" if templates == 0 else
       "%.1f; one per height means the tree-reduction minimum is still all that is reported"
       % (gpu_submits / templates)))
check(cpu_submits > 0 and cpu_hps > 0, "the control actually ran",
      "rival submitted %d at %.1f H/s; without it the split has nothing to measure against"
      % (cpu_submits, cpu_hps))
check(window > 0 and actual >= 0.90, "PPLNS window share",
      "%.2f%%; the fixed AMD card took 99.8%% at real difficulty" % (100.0 * actual))
check(gpu_hps > 0 and cpu_hps > 0 and actual >= 0.90 * expected, "window tracks hashrate",
      ("hashrate unreadable in one of the logs" if not (gpu_hps and cpu_hps) else
       "got %.3f%% of the window on %.3f%% of the hashrate" % (100.0 * actual, 100.0 * expected)))

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
    print("NOTE: the share list filled up, so the kernel counted more payable nonces than")
    print("one batch can hand back. That is the fix WORKING and saying so, and the session")
    print("figure above is income the miner did not claim. The remedy is a HARDER share")
    print("target, which means a LOWER share_bits in Cell 5. Raising it makes shares easier")
    print("and overflows sooner; past the point where the derivation saturates it does")
    print("nothing at all, because the ceiling is the chain's and not yours.")
```

### Reading the output

- **All five PASS.** The port is good: the kernel appends every hit, the host
  reads the live prefix, the CPU re-hash accepted every entry, and the pool
  credited the card in proportion to its hashrate.
- **Submission volume FAILs, hashrate looks fine.** The original defect exactly as
  it was on AMD. The kernel is still reporting only the reduction minimum: check
  that `share_capacity` reaches the kernel non-zero, that is, that the miner
  really is pooled.
- **Window share FAILs while submission volume PASSes.** Submissions are being
  made but not credited. Look at `kind=stale` in Cell 6 and at the pool log: a
  submit-gate or template-freshness problem, not a kernel problem.
- **"the control actually ran" FAILs.** The rival was not a control. Nothing else
  in the report can be trusted, because a dead rival and a perfect result are the
  same numbers.
- **A `GPU integrity error` in the miner log.** The CPU could not reproduce a hash
  the card reported, or the card listed a nonce whose hash does not beat the share
  target. Every share list entry is re-hashed on the CPU before it can reach the
  pool, and one bad entry fails the whole batch by design. Hardware or kernel
  fault, never something to work around.

### Why the absolute numbers differ from AMD

The 153,467 figure came from a gfx1201 at 205 MH/s. A T4 is far slower, so its
absolute submission count will be lower. The window-share and hashrate-share lines
are the hardware-independent test and are the ones to trust: they compare this
card against a CPU thread on the same box, in the same PPLNS window, which is
exactly how the defect was found.

---

## What this rig does not show

Payouts do not confirm here, and that is a property of the setup rather than a
defect. At mainnet difficulty neither miner wins a block, so the pool has no
coinbase to mature and nothing to settle. Worker balances staying at `0:0` is the
expected result, not a failure of the settlement path.

Nothing in this document adds a second block producer either. `[miner] enable` on
the node does NOT start a node-side hasher: it gates the two miner API routes and
sizes the transaction pool, and that is all. The settlement path is exercised
separately, on a rig where blocks can actually be won.
