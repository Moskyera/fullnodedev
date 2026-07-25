# CUDA end-to-end on Colab: node + payout pool + CUDA miner

Same scenario the AMD gfx1201 rig ran locally, so the two are directly
comparable. Each cell is standalone; a Colab `!` cell is a fresh shell, so the
environment is re-sourced every time.

Pick a GPU runtime first: Runtime -> Change runtime type -> T4 GPU.

What this proves, in order: the CUDA kernels are byte-correct, a CUDA miner wins
real blocks, and the full money path (shares -> PPLNS -> maturity -> payout) runs
on NVIDIA exactly as it does on AMD.

---

## Cell 1: clone and build

```python
!nvidia-smi --query-gpu=name,memory.total,driver_version --format=csv,noheader
!test -d /content/fullnodedev || git clone --depth 1 -b feat/pool-directory-cuda-ptx-panel https://github.com/Moskyera/fullnodedev.git /content/fullnodedev
!test -f "$HOME/.cargo/env" || (curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal)
!cd /content/fullnodedev && . "$HOME/.cargo/env" && export CUDA_PATH=/usr/local/cuda && export PATH=$PATH:/usr/local/cuda/bin && \
  cargo build --release --features cuda --bin fullnode --bin poworker 2>&1 | tail -5 && \
  cargo build --release -p pool-spike --bin pool-server 2>&1 | tail -3
```

Expect `Finished release profile`. The first build takes roughly 7 to 8 minutes.

---

## Cell 2: correctness before throughput

A fast miner that computes the wrong hash is worth nothing, so pin the kernels
against the CPU implementation first.

```python
!cd /content/fullnodedev && . "$HOME/.cargo/env" && export CUDA_PATH=/usr/local/cuda && export PATH=$PATH:/usr/local/cuda/bin && \
  cargo test -p x16rs-cuda --release 2>&1 | tail -12
```

Expect 4 passing tests, including `cuda_matches_cpu_across_many_inputs`, which is
the differential one: every GPU hash must equal the CPU hash byte for byte.
If this fails, stop. Nothing below is meaningful.

---

## Cell 3: configs

`difficulty_adjust_blocks` is deliberately 8 rather than the 288 default. At
bootstrap difficulty the share target and the block target coincide, so the pool
can never return `kind: "share"` and the PPLNS accounting cannot be exercised at
all. Shrinking the window makes ASERT engage after 9 blocks, so the run reaches
real difficulty in minutes instead of hours.

```python
import pathlib
D = pathlib.Path("/content/fullnodedev/target/release")

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
reward = 1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS
message = hbit-colab
""")

(D/"poworker.config.ini").write_text("""connect = 127.0.0.1:18082
pool_worker = 1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS
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
""")
print("configs written to", D)
```

---

## Cell 4: run node + pool + CUDA miner

Everything runs in ONE cell so the three processes genuinely overlap. A previous
attempt failed because the node had already exited before the miner started.

```python
import subprocess, os, time, json, urllib.request
D = "/content/fullnodedev/target/release"
env = dict(os.environ, LD_LIBRARY_PATH="/usr/local/cuda/lib64:" + os.environ.get("LD_LIBRARY_PATH",""))
subprocess.run("pkill -9 fullnode; pkill -9 poworker; pkill -9 pool-server; sleep 3; rm -rf %s/hacash_*_data %s/pool-wallet.key*" % (D,D), shell=True)

def get(url, t=3):
    return json.loads(urllib.request.urlopen(url, timeout=t).read().decode())

node = subprocess.Popen(["./fullnode"], cwd=D, stdout=open("/content/node.log","w"),
                        stderr=subprocess.STDOUT, env=env, start_new_session=True)
for _ in range(60):
    try:
        print("NODE UP height =", get("http://127.0.0.1:18080/query/latest")["height"]); break
    except Exception: time.sleep(1)

pool = subprocess.Popen(["./pool-server","http://127.0.0.1:18080","pool-wallet.key",
                         "127.0.0.1:18082","24","testnet:8:10","120"], cwd=D,
                        stdout=open("/content/pool.log","w"), stderr=subprocess.STDOUT,
                        env=env, start_new_session=True)
time.sleep(8)
print(open("/content/pool.log").read()[-600:])

miner = subprocess.Popen(["./poworker"], cwd=D, stdout=open("/content/miner.log","w"),
                         stderr=subprocess.STDOUT, env=env, start_new_session=True)
print("CUDA miner started, sampling for 10 minutes...")
for i in range(10):
    time.sleep(60)
    s = get("http://127.0.0.1:18082/stats")
    h = get("http://127.0.0.1:18080/query/latest")["height"]
    w = " ".join("%s=%d" % (a[:8], n) for a, n in s["workers"])
    print("t+%2dmin node_h=%-4d diff=%-12d shares=%-5d pend=%d orph=%d conf=%d [%s]"
          % (i+1, h, s["difficulty"], s["accepted_shares"], s["blocks_pending"],
             s["blocks_orphaned"], s["blocks_confirmed"], w))
for p in (miner, pool, node): p.terminate()
```

Watch `diff` leave 4294967294: that is ASERT engaging and the point where shares
become distinct from blocks.

---

## Cell 5: the numbers that matter

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

Compare against the AMD gfx1201 baseline (205 MH/s, 333 submits: 281 share,
15 block, 37 stale, 0 failed):

- `kind=share` should dominate once ASERT has engaged. If it is 0 while
  `kind=block` is high, the chain never left bootstrap difficulty and the PPLNS
  path was not exercised.
- `SUBMIT FAILED` should be 0 and `kind=stale` should be a small fraction. A
  large stale count means the submit gate is not settling dead templates.
- One `[Mining] height N settled` line per template, not thousands of rejections.
  Thousands of submits for a handful of accepted blocks is the pipeline jam that
  cost roughly half the hashrate before it was fixed.
- `blocks_orphaned` above zero is normal and healthy: it means the pool noticed
  its own block losing a race and did not pay out on it.
- `holding back N unit(s) ... not yet buried 16 deep` is the coinbase maturity
  guard. Payouts lag block discovery by design.

Known open issue at the time of writing: a payout can be accepted by the node
and still never confirm, because no block produced through the miner API has
ever contained a transaction. If the worker balances stay at `0:0` after a
settlement line, that is this issue and not a CUDA problem.
