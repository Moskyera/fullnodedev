# CUDA on Colab: prove the kernels, then measure them

Pick a GPU runtime first: Runtime -> Change runtime type -> T4 GPU.

The run is **build, prove, measure**, in that order, and it does not reach
"measure" if "prove" fails.

| Cell | What it does | Rough cost on a free T4 |
| --- | --- | --- |
| 0 | Is this runtime actually a GPU runtime? | seconds |
| 1 | Fetch the exact commit, and check it contains the CUDA gate | 1 to 3 min |
| 2 | **The gate.** Every GPU hash equals `x16rs::block_hash`, byte for byte, and the gate is shown to catch kernels that are wrong on purpose | 25 to 45 min |
| 3 | The `x16rs-cuda` test suite, which covers the pool share list itself | 3 to 6 min |
| 4 | Build `fullnode`, `poworker`, `hbit-pool-server` | 5 to 12 min |
| 5 | Configs for the node, the CUDA miner and the CPU rival | seconds |
| 6 | Sync the node to the real mainnet tip | 30 min to 2 h |
| 7 | Run the pool, the CUDA miner and the CPU rival, and sample | ~11 min |
| 8 | Raw submission counts | seconds |
| 9 | The verdict: is the card paid for what it mines? | seconds |

Cells 0 to 3 are the correctness half and need nothing but a GPU. Cells 4 to 9
are the payment half and need the real chain. If your session is short, run 0 to
3, keep the log, and come back for the rest: they are independent claims.

The times in that table are estimates, not measurements. The build times are
extrapolated from a 32-thread Windows box where one kernel-tree rebuild of
`x16rs-cuda` plus its dependents took 1 minute 14 seconds; a 2-vCPU Colab VM will
be several times slower, and the gate does four of those rebuilds. The Cell 6
range is from the one completed mainnet sync on record. Only Cell 7 is fixed by
construction, at `SAMPLE_MINUTES` plus about a minute of setup.

Nothing here spends money. The node syncs and validates public blocks, the pool
holds a throwaway wallet, and at mainnet difficulty nothing attached to it is
going to win a block.

---

## Why the gate comes first

A hashrate from a kernel nobody checked is not a result. It is a number.

The equivalence gate compares **every** GPU hash in a window against
`x16rs::block_hash`, the CPU reference, at repeat 1, 4, 8 and 16, across three
launch shapes. It does that by borrowing the pool share list with an all-ones
target, so a whole 1024-nonce window comes off the card at once. On top of that
it runs the production launch shape (589,824 nonces, far more than the share
list holds) and checks the kernel's own hit counter against the CPU's sorted
oracle at 255 rank thresholds, which reads every one of those hashes.

That gate found real defects three separate times on the AMD card, and it is the
only reason a 50% speed change there was believed.

The gate also proves it can fail. `scripts/x16rs_gate_trees.py` writes three
copies of the kernel tree with deliberate defects in them:

| tree | defect | what the gate should say |
| --- | --- | --- |
| A | shabal's counter starts at 2 instead of 1 | `ALGORITHM: shabal (13)` |
| B | the barrier inside the repeat loop is deleted | `NO single algorithm is implicated` (correct: it is a race) |
| C | one bit flipped in blake's IV | `ALGORITHM: blake (0)` |

`x16rs-cuda/cuda/block_miner.cu` includes the algorithm sources straight out of
`x16rs/opencl`, so the same three trees drive both backends. OpenCL takes a tree
at runtime; CUDA takes it through `X16RS_CUDA_KERNEL_DIR` and a rebuild.

---

## Before you start: the branch has to contain the gate

**The CUDA half of the gate is newer than the last pushed commit.** At the time
this document was written, `feat/pool-directory-cuda-ptx-panel` on the fork was
at `3248146`, and that commit has:

- no `CudaBackend` in `app/src/x16rs_gate.rs` (`git show 3248146:app/src/x16rs_gate.rs | grep -c cuda` is `0`),
- no `--backend` flag in `src/bin/x16rs_gate.rs`,
- no `X16RS_CUDA_KERNEL_DIR` in `x16rs-cuda/build.rs`,
- no `colab_cuda_gate.sh` at all.

Clone that commit and Cell 2 has nothing to run. **Push the branch with the CUDA
gate on it first.** Cell 1 checks for each of those things by name and stops with
the list of what is missing, so you find out in the first two minutes rather than
after a build.

---

## What PASS and FAIL look like

Every cell that can fail prints a bordered `STOP` block and then raises, so the
notebook halts instead of scrolling on. There is no way to get to Cell 9 past a
failed Cell 2.

**Cell 2 PASS.** The last lines of the gate are:

```
==============================================
 RESULT: PASS   (wall +31:12)
  - the CUDA kernels match x16rs::block_hash byte for byte
  - the gate caught 3/3 deliberately broken kernels
==============================================
```

and the cell then prints `GATE: PASS`. Inside the run, the equivalence report
that licenses it looks like this (`mismatches : 0`, and no algorithm marked
`NEVER TESTED`).

The counts below are from the AMD gfx1201 run at these same parameters, not from
a T4. They are quoted because they are determined by the parameters and not by
the card: the corpus, the three exhaustive shapes, the rank thresholds and the
share-list sizes are all fixed, so a T4 at `--headers 4 --prod-batches 1
--work-groups 48 --unit-size 48` should print the same figures. If yours differ,
the parameters differ. **No part of this report has been observed on an NVIDIA
device by anyone here.**

```
================ BYTE-EQUIVALENCE GATE ================
  backend / device : cuda / <the T4>
  hashes compared byte-for-byte : 180499
  exhaustive batches (ENTIRE window dumped and compared) : 48
  production-shape windows : 1 (589824 nonces, all CPU-hashed)
  production full-window count checks : 132 (each reads all 589824 GPU hashes)
  production best-hash reductions proved minimal : 1
  mismatches : 0
  algorithm coverage (rounds executed, CPU-derived):
     0 blake              ...
    ...
  RESULT: PASS
```

**Cell 2 FAIL.** Three different things print differently, and they mean
different things:

| What you see | What it means | What to do |
| --- | --- | --- |
| `RESULT: FAIL` with `mismatches : N` and a list of nonces | The card's hashes are not the CPU's | Stop. Do not mine, do not quote a hashrate. The report names the algorithm when the evidence allows it |
| `RESULT: FAIL` with `reason=the gate could not open or run the CUDA device (exit 4)` | Nothing was compared | Usually no GPU attached. Cell 0 should have caught it |
| `RESULT: FAIL` with `fault X was NOT caught` | The kernels agreed with the CPU, but the gate could not detect a kernel that is broken on purpose, so that agreement is not evidence | See the note on fault B below |

**`RESULT: PASS-RACE-NOT-REPRODUCED`.** Fault B is a deleted barrier, which is a
data race. A race is caught because the hardware happened to interleave badly.
It was caught on an AMD gfx1201; **whether an NVIDIA scheduler reproduces it has
never been observed here**, because there is no NVIDIA GPU on the machine this
was written on. If A and C are caught and B is not, the gate's arithmetic is
demonstrably working and the honest reading is that this card did not race. Re-run
with `ALLOW_RACE_MISS=1` to continue; the result string changes to
`PASS-RACE-NOT-REPRODUCED` and stays that way in the summary file, so a run that
used it can never be quoted later as a clean PASS.

**Cell 9 PASS/FAIL** is a separate claim about payment, not about correctness.
See "Reading the output" at the end.

---

## What in this document has actually been run

The machine this was written on has an AMD RX 9070 XT and **no NVIDIA GPU**, so
nothing here has been executed against a real CUDA device. Being precise about
that is the point of the gate.

Verified on the authoring machine (Windows, CUDA 13.3, no NVIDIA device):

- `cargo build --release --features cuda --bin x16rs_gate` compiles, with
  `block_miner.cu` built by a real nvcc for `sm_75` (a T4), `sm_86`, `sm_89` and
  a `compute_89` PTX fallback. `nvcc --list-gpu-code` on 13.3 still lists
  `sm_75`, so the T4 arch is not one of the ones a modern toolkit dropped.
- Setting `X16RS_CUDA_KERNEL_DIR` to a fault tree forces a real rebuild (1m14s)
  rather than reusing the previous object file, and unsetting it rebuilds back.
  So the fault-injection mechanism the gate's step 2 depends on works.
- `x16rs_gate equiv --backend cuda` with no NVIDIA device exits **4** with
  "the installed NVIDIA driver is older than the CUDA runtime this binary was
  built against, or there is no NVIDIA driver at all (code 35)". It does not
  crash: the null return from `cudaGetErrorString(35)` that used to segfault is
  handled.
- `colab_cuda_gate.sh` on a box with no GPU stops at step 0 with the bordered
  FAIL banner and writes `result=FAIL` with a reason to
  `latest-gate-summary.txt`. Its build-failure path was exercised too.
- Every Python cell in this document compiles. The Cell 1 content check was run
  against both trees: it passes on a tree that has the CUDA gate and, against
  commit `3248146`, names five of the six missing pieces.

**Not run anywhere, and unrunnable here:**

- Cells 2, 3 and 6 through 9 as a whole. No NVIDIA device.
- The equivalence PASS itself on CUDA. The OpenCL half of this gate passes on the
  AMD card and the CUDA half shares every line of judgement with it, but "the
  CUDA kernels agree with the CPU" is a claim only a T4 run can make.
- Whether an NVIDIA scheduler reproduces fault B. See the note above.
- The Drive parking in Cell 6b.

---

## Cell 0: is this actually a GPU runtime?

```python
import os, shutil, subprocess

def die(msg):
    """Stop the notebook loudly. A bare `!command` that exits nonzero does NOT
    stop a Colab notebook: IPython ignores the status and the next cell runs on
    whatever the last successful run left behind. Everything below therefore
    raises rather than trusting an exit code nobody reads."""
    lines = msg.strip().splitlines()
    print("\n" + "#" * 72)
    print("#  STOP")
    for line in lines:
        print("#  " + line)
    print("#" * 72)
    raise RuntimeError(lines[0])

if shutil.which("nvidia-smi") is None:
    die("""This runtime has no NVIDIA GPU.
Runtime -> Change runtime type -> T4 GPU, then run this cell again.
Every number below would be either an error or a measurement of nothing.""")

print(subprocess.run(["nvidia-smi", "--query-gpu=name,memory.total,driver_version",
                      "--format=csv"], capture_output=True, text=True).stdout)

nvcc = shutil.which("nvcc") or "/usr/local/cuda/bin/nvcc"
if os.path.exists(nvcc):
    print(subprocess.run([nvcc, "--version"], capture_output=True, text=True).stdout)
else:
    die("""No nvcc. The CUDA toolkit is normally at /usr/local/cuda on Colab.
Without it x16rs-cuda/build.rs compiles a crate with NO kernels in it: every
device call returns NotCompiled, and a gate run would prove nothing. The gate
refuses to start in that state rather than report a pass over zero hashes.""")

print("CPU cores (the gate hashes its CPU oracle on these):", os.cpu_count())
```

Two cores is normal on the free tier and is enough. The CPU oracle for one
production window is 589,824 hashes at repeat 16, which is the largest single CPU
cost in the gate.

---

## Cell 1: get exactly the code you mean to test, and check it is the right code

Two separate failures live here, and the second is the one that has actually bitten.

`git clone` is skipped when the directory already exists, so a Colab session that
survived a runtime restart silently re-tests an old commit. The fetch and reset
fix that, and the commit is printed so it is in the log.

But landing on the right commit is not the same as landing on code that contains
the gate. The CUDA gate is newer than the last push, so the check below looks for
each piece **by name** and says which one is missing.

```python
import os, subprocess, pathlib

def die(msg):
    lines = msg.strip().splitlines()
    print("\n" + "#" * 72)
    print("#  STOP")
    for line in lines:
        print("#  " + line)
    print("#" * 72)
    raise RuntimeError(lines[0])

REPO   = "https://github.com/Moskyera/fullnodedev.git"
BRANCH = "feat/pool-directory-cuda-ptx-panel"
D      = "/content/fullnodedev"

def sh(cmd, cwd=None, what=None):
    print("$", cmd)
    rc = subprocess.run(cmd, shell=True, cwd=cwd, executable="/bin/bash").returncode
    if rc != 0:
        die("%s failed (exit %d).\ncommand: %s" % (what or "a shell step", rc, cmd))

if not os.path.isdir(D + "/.git"):
    sh("git clone --depth 1 -b %s %s %s" % (BRANCH, REPO, D), what="git clone")

# Force the checkout onto the tip of the branch, whatever it was before.
sh("git fetch --depth 1 origin %s" % BRANCH, cwd=D, what="git fetch")
sh("git reset --hard FETCH_HEAD", cwd=D, what="git reset")

head = subprocess.run("git log -1 --format='%H%n%h%n%ad%n%s' --date=iso",
                      shell=True, cwd=D, capture_output=True, text=True).stdout.split("\n")
print()
print("commit  :", head[0])
print("short   :", head[1])
print("date    :", head[2])
print("subject :", head[3])

dirty = subprocess.run("git status --porcelain", shell=True, cwd=D,
                       capture_output=True, text=True).stdout.strip()
print("tree    :", "clean" if not dirty else "DIRTY\n" + dirty)

# Landing on a commit is not the same as landing on the code this notebook runs.
# Each entry is (path, text that must appear in it, why it matters).
REQUIRED = [
    ("scripts/mining-nvidia/colab_cuda_gate.sh", None,
     "the gate runner Cell 2 calls"),
    ("app/src/x16rs_gate.rs", "CudaBackend",
     "the CUDA backend of the gate. Without it there is no --backend cuda, and the "
     "gate can only test OpenCL, which this runtime does not have"),
    ("src/bin/x16rs_gate.rs", "--backend",
     "the flag that selects the backend"),
    ("x16rs-cuda/build.rs", "X16RS_CUDA_KERNEL_DIR",
     "the knob that rebuilds the CUDA kernels from a deliberately broken tree. "
     "Without it the CUDA gate can never be shown to FAIL, and a gate that has only "
     "been seen to pass proves nothing"),
    ("x16rs/opencl/x16rs.cl", "X16RS_H_BLAKE_INIT",
     "blake's initialisation vector, exported so block_miner.cu reads it instead of "
     "carrying its own copy. Without this the fault-injection proof is hollow: fault C "
     "flips a bit in x16rs.cl, block_miner.cu keeps the old value, the PTX is "
     "byte-identical, and the gate returns PASS for a kernel broken on purpose"),
    ("scripts/x16rs_gate_trees.py", "faults",
     "the three fault trees"),
]

missing = []
for path, needle, why in REQUIRED:
    p = pathlib.Path(D) / path
    if not p.is_file():
        missing.append("%s is not in this commit\n      (%s)" % (path, why))
    elif needle and needle not in p.read_text(errors="replace"):
        missing.append("%s exists but does not contain %r\n      (%s)" % (path, needle, why))

if missing:
    die("""This commit predates the CUDA gate, so Cell 2 has nothing to run.

Missing:
    """ + "\n    ".join(missing) + """

Push the branch that carries the CUDA gate and re-run this cell. Measuring a
hashrate off this commit is exactly the mistake the gate exists to prevent.""")

print("\nall %d required pieces of the CUDA gate are present in this commit" % len(REQUIRED))

# Rust, once, for every build cell below.
if not os.path.exists(os.path.expanduser("~/.cargo/env")):
    sh("curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal", what="rustup install")

# ONE profile for the whole notebook. Two reasons, and the second is the
# expensive one:
#
#   * the workspace ships lto = "thin" and codegen-units = 1, which on a free
#     Colab VM is slow and can be killed for memory;
#   * cargo keys its build cache on the profile, so a gate cell that lowers LTO
#     and a miner cell that does not would compile the entire dependency tree
#     TWICE. colab_cuda_gate.sh exports exactly these values, so matching them
#     here is what makes Cell 4 reuse Cell 2's work.
#
# The cost is that absolute hashrates from this run are not comparable with a
# shipping-profile build. The GPU-versus-CPU split in Cell 9 still is, because
# both sides are built the same way, and the GPU kernels are compiled by nvcc at
# -O3 either way.
CARGO_ENV = {
    "CARGO_TERM_COLOR": "always",
    "CARGO_INCREMENTAL": "0",
    "CARGO_PROFILE_RELEASE_LTO": "false",
    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "16",
    "CARGO_PROFILE_RELEASE_OPT_LEVEL": "2",
    "CARGO_PROFILE_RELEASE_STRIP": "false",
    "CUDA_PATH": "/usr/local/cuda",
    "PATH": os.environ["PATH"] + ":/usr/local/cuda/bin:" + os.path.expanduser("~/.cargo/bin"),
    "LD_LIBRARY_PATH": "/usr/local/cuda/lib64:" + os.environ.get("LD_LIBRARY_PATH", ""),
}
os.environ.update(CARGO_ENV)
print("build environment pinned; every later build cell inherits it")
```

---

## Cell 2: the gate. Nothing below this is worth reading until it passes

This is the long one. It builds the gate, proves the shipping kernels equal the
CPU byte for byte, then rebuilds three more times against kernel trees that are
wrong on purpose and requires the gate to catch each one.

**It is resumable.** Each step writes a marker under
`target/gate-state/<fingerprint>/` when it succeeds, and a re-run skips it. The
fingerprint covers the commit, the kernel sources and the gate sources, so
editing any of them starts over by itself. A T4 session that dies during fault B
costs you fault B, not the whole run. `RESUME=0` forces everything to be redone.

It prints a heartbeat every 60 seconds during a compile, with elapsed time, so a
quiet cell is distinguishable from a dead one.

If you are short on session and only want the equivalence half, `SKIP_FAULTS=1`
cuts it to roughly a quarter of the time. The result string becomes
`PASS-UNPROVEN`, which is honest: the kernels passed, the gate itself was not
exercised.

```python
import os, subprocess, pathlib

def die(msg):
    lines = msg.strip().splitlines()
    print("\n" + "#" * 72)
    print("#  STOP")
    for line in lines:
        print("#  " + line)
    print("#" * 72)
    raise RuntimeError(lines[0])

D = "/content/fullnodedev"

GATE_ENV = dict(os.environ)
# GATE_ENV["SKIP_FAULTS"] = "1"       # equivalence only, no fault injection
# GATE_ENV["ALLOW_RACE_MISS"] = "1"   # see "What PASS and FAIL look like" above
# GATE_ENV["RESUME"] = "0"            # redo every step from scratch

# Streamed line by line rather than captured, so the heartbeat is visible while
# it runs. A captured build looks identical to a hung one for twenty minutes.
proc = subprocess.Popen(
    ["bash", "scripts/mining-nvidia/colab_cuda_gate.sh"],
    cwd=D, env=GATE_ENV, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    text=True, bufsize=1)
for line in proc.stdout:
    print(line, end="")
rc = proc.wait()

summary = pathlib.Path(D) / "scripts/mining-nvidia/colab-results/latest-gate-summary.txt"
if not summary.is_file():
    die("The gate wrote no summary file, so it died before reaching a verdict "
        "(exit %d). Its output is above." % rc)

kv = dict(line.split("=", 1) for line in summary.read_text().splitlines() if "=" in line)
result = kv.get("result", "MISSING")

# The summary file is overwritten in place, so an aborted run can leave the
# PREVIOUS run's verdict sitting there looking current. Tie it to this commit.
# The fallback matches what the gate script writes when there is no .git, so the
# zip route (see COLAB-T4.md, Option B) compares equal instead of always dying.
head = subprocess.run("git rev-parse --short HEAD", shell=True, cwd=D,
                      capture_output=True, text=True).stdout.strip() or "not-a-git-checkout"
if kv.get("commit") != head:
    die("The gate summary says commit %s, but this checkout is %s, so that file is "
        "left over from an earlier run and its verdict is not about this code."
        % (kv.get("commit", "<no commit line>"), head))

print()
print("=" * 72)
if rc != 0 or result.startswith("FAIL") or result == "MISSING":
    die("""GATE: %s   (exit %d)
reason: %s

The CUDA kernels on this card are NOT proven to compute x16rs::block_hash.
Do not run the miner and do not report a hashrate from this build.
Full log: %s""" % (result, rc, kv.get("reason", "see the output above"), kv.get("log", "?")))

print("GATE:", result)
print("faults caught:", kv.get("faults_caught", "n/a"))
print("commit:", kv.get("commit"), " log:", kv.get("log"))
if result != "PASS":
    print()
    print("!" * 72)
    print("!  This is NOT a clean PASS. It is:", result)
    if result == "PASS-UNPROVEN":
        print("!  The shipping kernels matched the CPU, but SKIP_FAULTS=1 meant the gate")
        print("!  was never shown to be able to catch a broken kernel on this box.")
    if result == "PASS-RACE-NOT-REPRODUCED":
        print("!  The arithmetic faults were caught. The data-race fault (%s) did not"
              % kv.get("race_not_reproduced", "B"))
        print("!  reproduce on this card and was waived by ALLOW_RACE_MISS=1.")
    print("!  Quote the result string, not the word PASS.")
    print("!" * 72)
print("=" * 72)
```

---

## Cell 3: the `x16rs-cuda` suite, which covers what the gate does not

The gate proves hash equivalence. It does not exercise the share list's
bookkeeping: overflow when more nonces qualify than the list can hold, the
counter not leaking from a pooled batch into the next solo one, the readback
never reading past the counter. Those are in `x16rs-cuda`.

Two traps make a green run here meaningless, and the cell checks for both.

`--features cuda` is not optional: every GPU test is behind it, and without it
the run passes on the single CPU test. Worse, `gpu_share_list_tests` is
`#[cfg(all(test, cuda_available))]`, and `cuda_available` is set by `build.rs`
only when it actually found nvcc. Without nvcc that module is **not compiled at
all**, so its absence is silent: `cargo test` prints a tidy `ok` for the tests
that remain. The only reliable check is that the test names are present in the
output, so that is what this cell asserts.

```python
import os, subprocess, pathlib

def die(msg):
    lines = msg.strip().splitlines()
    print("\n" + "#" * 72)
    print("#  STOP")
    for line in lines:
        print("#  " + line)
    print("#" * 72)
    raise RuntimeError(lines[0])

D = "/content/fullnodedev"

proc = subprocess.Popen(
    ["cargo", "test", "-p", "x16rs-cuda", "--release", "--features", "cuda",
     "--", "--nocapture"],
    cwd=D, env=os.environ, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    text=True, bufsize=1)
out = []
for line in proc.stdout:
    print(line, end="")
    out.append(line)
rc = proc.wait()
text = "".join(out)

if rc != 0:
    die("cargo test -p x16rs-cuda failed (exit %d). Its output is above." % rc)

# Each of these must have RUN. Not "not failed": run.
MUST_RUN = [
    ("cuda_matches_cpu_across_many_inputs",
     "the differential test: 4096 inputs at repeat 1 and 512 at repeat 16"),
    ("cuda_batch_matches_cpu",
     "the batch path against the CPU"),
    ("cuda_genesis_block_hash_when_available",
     "the real mainnet genesis vector"),
    ("the_share_list_matches_the_cpu_and_leaves_the_best_result_untouched",
     "the share list itself: solo returns the CPU's single best with an empty list; "
     "an easy target makes the counter see the whole window while the list stores its "
     "capacity and reports the rest as overflow; a strict target returns exactly the "
     "payable nonces; and a pool batch does not leak its counter into the next solo batch"),
    ("the_blake_iv_is_not_duplicated_into_the_cuda_source",
     "the guard that keeps fault C meaningful: if block_miner.cu ever carries its own "
     "copy of blake's IV again, patching x16rs.cl stops changing the PTX and the "
     "fault-injection proof goes hollow"),
]
absent = [(name, why) for name, why in MUST_RUN if name not in text]
if absent:
    die("""These tests did not run, so this green result does not cover them:

    """ + "\n    ".join("%s\n      (%s)" % (n, w) for n, w in absent) + """

The usual cause is that build.rs did not find nvcc, so cfg(cuda_available) is
unset and the GPU test modules were never compiled. Check for the cargo warning
"Using CUDA Toolkit at ..." in the output above; if instead it says "CUDA Toolkit
not found", set CUDA_PATH and re-run Cell 1.""")

for marker in ("skipping", "no usable CUDA device", "CUDA kernels not compiled"):
    if marker in text:
        die("""The suite reported %r, which means a GPU test declined to run and still
counted as a pass. Nothing here proves anything about the card.""" % marker)

print("\nall %d GPU tests ran on the device" % len(MUST_RUN))
```

---

## Cell 4: build the mining binaries

`fullnode`, `poworker` and `x16rs_gate` are all binaries of the same root
package, so this reuses everything Cell 2 already compiled, provided the profile
matches (Cell 1 pinned it).

```python
import os, subprocess

def die(msg):
    lines = msg.strip().splitlines()
    print("\n" + "#" * 72)
    print("#  STOP")
    for line in lines:
        print("#  " + line)
    print("#" * 72)
    raise RuntimeError(lines[0])

D = "/content/fullnodedev"

def build(args, label):
    print(">>>", label)
    proc = subprocess.Popen(args, cwd=D, env=os.environ, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, bufsize=1)
    tail = []
    for line in proc.stdout:
        print(line, end="")
        tail.append(line)
    if proc.wait() != 0:
        die("%s FAILED.\nCargo leaves the previous binary in place when a build fails, "
            "so continuing would measure whatever was built last time." % label)

build(["cargo", "build", "--release", "--features", "cuda",
       "--bin", "fullnode", "--bin", "poworker"], "fullnode + poworker (CUDA)")
build(["cargo", "build", "--release", "-p", "hbit-pool",
       "--bin", "hbit-pool-server"], "hbit-pool-server")

for name in ("fullnode", "poworker", "hbit-pool-server", "x16rs_gate"):
    p = os.path.join(D, "target/release", name)
    print("%-18s %s" % (name, "%d bytes" % os.path.getsize(p) if os.path.exists(p) else "MISSING"))
```

---

## Cell 5: configs

The node is a plain mainnet node with mining ENABLED. That flag does not make the
node hash anything: its only effects are to gate the two miner API routes
(`/query/miner/pending` and `/submit/miner/success`) and to size the transaction
pool. The pool needs those routes to fetch templates, which is the whole reason it
is on.

The second worker config is the point of the exercise. On the AMD rig the defect
was invisible in the miner's own numbers: the card reported a healthy hashrate
while a single-threaded CPU miner beside it took the entire PPLNS window. So this
run puts that CPU rival back, on its own address, and Cell 9 measures the split.

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

## Cell 6: sync the node to the mainnet tip

### Why this runs against mainnet, and not a local testnet

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
Cell 9 was taken.

### The cell

This is the longest wait in the notebook. It downloads and validates the real
chain. Leave it running; it reports progress every 30 seconds and stops on its
own.

It resumes rather than starting over **as long as the VM lives**: the chain data
sits in `/content/fullnodedev/target/release/hacash_mainnet_data`, so re-running
this cell after an interrupt picks up where it stopped. It does NOT survive the
VM being recycled, because `/content` goes with it. Cell 6b parks a completed
sync on Drive if you expect to come back.

Expect about 2.7 GB of chain data, measured from a completed sync of the same
chain, which is comfortably inside Colab's disk. The time is dominated by
download and validation rather than by anything on the GPU.

```python
import subprocess, os, time, json, urllib.request

def die(msg):
    lines = msg.strip().splitlines()
    print("\n" + "#" * 72)
    print("#  STOP")
    for line in lines:
        print("#  " + line)
    print("#" * 72)
    raise RuntimeError(lines[0])

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

def datadir_mb():
    total = 0
    for root, _, files in os.walk(os.path.join(D, "hacash_mainnet_data")):
        for f in files:
            try:
                total += os.path.getsize(os.path.join(root, f))
            except OSError:
                pass
    return total / (1024.0 * 1024.0)

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
    die("The node never answered on 18080. Its log is above.")

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
t0 = time.time()
print("syncing the real chain. This is the slow part; leave it running.")
print("re-running this cell after an interrupt RESUMES; it does not start over.")
for i in range(240):                      # 240 x 30s = up to two hours
    time.sleep(30)
    if node.poll() is not None:
        print(open("/content/node.log").read()[-2000:])
        die("The node process exited during sync. Its log is above.")
    try:
        h = get("http://127.0.0.1:18080/query/latest")["height"]
        t = get("http://127.0.0.1:18080/query/miner/pending").get("target_hash")
    except Exception as e:
        print("  poll failed (%s); the node is busy, continuing" % e)
        continue
    mins = (time.time() - t0) / 60.0
    if t is None:
        print("  t+%5.1fmin height %-8d (no template yet)" % (mins, h))
        continue
    n = lzbits(t)
    rate = "" if prev_h is None else "  +%d blocks/30s" % (h - prev_h)
    print("  t+%5.1fmin height %-8d work 2^%-3d %6.0f MB%s" % (mins, h, n, datadir_mb(), rate))
    if n >= NEED_BITS and prev_h is not None and (h - prev_h) < 5:
        synced = True
        break
    prev_h = h
if not synced:
    print(open("/content/node.log").read()[-1500:])
    node.terminate()
    die("""The node did not reach a stable tip at 2^%d work within two hours.
Its log is above; check connectivity to the boot nodes.
Re-running this cell resumes from the height it reached.""" % NEED_BITS)

tip = get("http://127.0.0.1:18080/query/latest")["height"]
work = lzbits(get("http://127.0.0.1:18080/query/miner/pending")["target_hash"])
json.dump({"tip": tip, "work_bits": work}, open("/content/sync.json","w"))
print()
print("SYNCED. tip height %d, a block costs 2^%d hashes." % (tip, work))
print("Leave this node running and go straight to Cell 7.")
```

---

## Cell 6b (optional): park the synced chain on Drive

Only worth it if you expect the VM to be recycled before you finish. Restoring
2.7 GB from Drive and syncing the delta beats validating 700k blocks again, but
it is not free either, so this is opt-in.

Run the archive half **after** Cell 6 reports SYNCED, and the restore half
**before** Cell 6 on a fresh VM.

```python
# --- restore (run BEFORE Cell 6 on a fresh VM) ---
import os, subprocess
from google.colab import drive
drive.mount("/content/drive")
ARCHIVE = "/content/drive/MyDrive/hacash-colab/hacash_mainnet_data.tar"
DEST    = "/content/fullnodedev/target/release"
if os.path.exists(ARCHIVE) and not os.path.exists(DEST + "/hacash_mainnet_data"):
    os.makedirs(DEST, exist_ok=True)
    print("restoring %.1f GB from Drive; this takes a while but beats a full resync"
          % (os.path.getsize(ARCHIVE) / 1e9))
    subprocess.run(["tar", "-xf", ARCHIVE, "-C", DEST], check=True)
    print("restored. Cell 6 will sync only the delta since the archive was made.")
else:
    print("nothing to restore" if not os.path.exists(ARCHIVE) else "chain data already present")
```

```python
# --- archive (run AFTER Cell 6 says SYNCED, with the node STOPPED) ---
# The node must not be writing while tar reads, or the archive is torn.
import os, subprocess
from google.colab import drive
drive.mount("/content/drive")
subprocess.run("pkill -9 -x fullnode; sleep 3", shell=True)
os.makedirs("/content/drive/MyDrive/hacash-colab", exist_ok=True)
subprocess.run(["tar", "-cf", "/content/drive/MyDrive/hacash-colab/hacash_mainnet_data.tar",
                "-C", "/content/fullnodedev/target/release", "hacash_mainnet_data"], check=True)
print("archived. Re-run Cell 6 to bring the node back up before Cell 7.")
```

---

## Cell 7: run the pool, the CUDA miner and the CPU rival

The node from Cell 6 must still be running. This cell does not start one.

`share_bits` is computed, not typed. The pool serves a share target eased from the
network target by `share_bits`, so what a share COSTS is
`work_bits - share_bits`. Too easy and credit measures HTTP round trips rather
than hashing; too hard and the sample sees almost no shares. Twenty bits leaves a
share costing about a million hashes: tens per second for the card, well under one
per second for a single CPU thread, which is exactly the spread being measured.

```python
import subprocess, os, time, json, urllib.request, re

def die(msg):
    lines = msg.strip().splitlines()
    print("\n" + "#" * 72)
    print("#  STOP")
    for line in lines:
        print("#  " + line)
    print("#" * 72)
    raise RuntimeError(lines[0])

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

# Delete last run's evidence BEFORE producing this run's. Cells 8 and 9 read these
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
    die("The chain is too easy for an honest share here; the pool would refuse "
        "and it would be right to. Re-run Cell 6 until the tip is real.")

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
        die("The pool exited during startup; its message is above.")
    if "caps it at" in poollog:
        die("The pool capped the share factor below what was asked, so the served "
            "share target is at its ceiling and the split would be meaningless.")

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
        die("The rival did not load " + RIVAL_CFG + "; its log is above.")
    if re.search(r"Create CUDA block miner worker|\[CUDA\] Device #", riv):
        die("The rival came up as a CUDA worker; it must be the CPU control.")
    if "Create CUDA block miner worker" not in gpu:
        print(gpu[:1200])
        die("The GPU miner never created a CUDA worker; its log is above.")

    print("CUDA miner and single-thread CPU rival running. Sampling for %d minutes..."
          % SAMPLE_MINUTES)
    samples = []
    for i in range(SAMPLE_MINUTES):
        time.sleep(60)
        # A dead miner must end the run, not produce ten minutes of zeroes. The
        # rival dying is the failure mode that reads as a PERFECT score.
        if miner.poll() is not None:
            die("The CUDA miner exited at minute %d." % (i + 1))
        if rival.poll() is not None:
            die("The CPU rival exited at minute %d, so there is no control." % (i + 1))
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

## Cell 8: the raw counts

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

## Cell 9: the share list, and whether the card is paid for what it mines

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

That run predates the gate, so it is a payment result and not a correctness one.
Nothing in it says the card computed the right hashes; Cell 2 is what says that.

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
undersampling path was not reached outside the Cell 3 unit test that covers it
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
    "final_stats.json is from run %s, not %s: Cell 7 aborted before finishing" % (
        stats.get("run_id"), run_id)
assert time.time() - os.path.getmtime("/content/final_stats.json") < 3600, \
    "these results are over an hour old; re-run Cell 7"

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
    print("This is a PAYMENT result. Correctness is Cell 2's claim, not this one's.")
else:
    print("RESULT: FAIL. Do not ship this. A failing window-share line with a")
    print("healthy hashrate is the original defect: the card mines and the pool")
    print("credits somebody else.")

if undersample:
    print()
    print("NOTE: the share list filled up, so the kernel counted more payable nonces than")
    print("one batch can hand back. That is the fix WORKING and saying so, and the session")
    print("figure above is income the miner did not claim. The remedy is a HARDER share")
    print("target, which means a LOWER share_bits in Cell 7. Raising it makes shares easier")
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
  made but not credited. Look at `kind=stale` in Cell 8 and at the pool log: a
  submit-gate or template-freshness problem, not a kernel problem.
- **"the control actually ran" FAILs.** The rival was not a control. Nothing else
  in the report can be trusted, because a dead rival and a perfect result are the
  same numbers.
- **A `GPU integrity error` in the miner log.** The CPU could not reproduce a hash
  the card reported, or the card listed a nonce whose hash does not beat the share
  target. Every share list entry is re-hashed on the CPU before it can reach the
  pool, and one bad entry fails the whole batch by design. Hardware or kernel
  fault, never something to work around. If Cell 2 passed and this appears,
  suspect the hardware.

### Why the absolute numbers differ from AMD

The 153,467 figure came from a gfx1201 at 205 MH/s. A T4 is far slower, so its
absolute submission count will be lower. The window-share and hashrate-share lines
are the hardware-independent test and are the ones to trust: they compare this
card against a CPU thread on the same box, in the same PPLNS window, which is
exactly how the defect was found.

Absolute hashrates from this notebook also carry the reduced build profile from
Cell 1 (LTO off, opt-level 2, chosen so free Colab can finish the build at all).
The GPU kernels are nvcc `-O3` regardless, so the CUDA figure moves little; the
CPU rival's does. The split is unaffected because both sides are built the same
way.

---

## Collect the evidence before the session dies

```python
from pathlib import Path
import shutil, time
OUT = Path("/content/colab-evidence-%s" % time.strftime("%Y%m%dT%H%M%S"))
OUT.mkdir()
for p in ["/content/node.log", "/content/pool.log", "/content/miner.log",
          "/content/cpu.log", "/content/final_stats.json", "/content/run_id.txt",
          "/content/sync.json"]:
    if Path(p).is_file():
        shutil.copy(p, OUT)
res = Path("/content/fullnodedev/scripts/mining-nvidia/colab-results")
if res.is_dir():
    shutil.copytree(res, OUT / "colab-results")
print("collected into", OUT)
print("\n".join(str(p) for p in sorted(OUT.rglob("*"))))
# Download from the file browser on the left, or:
# from google.colab import files; shutil.make_archive(str(OUT), "zip", OUT); files.download(str(OUT) + ".zip")
```

The gate log is the one to keep. It is the only artefact that says the hashes
were right, and it names the commit it says it about.

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

The gate does not test the miner's own launch shape end to end either. It tests
the kernel at that shape (`48x256x48` by default, and `PROD_WG`/`PROD_UNIT`
change it), but `poworker` in Cell 5 is configured `256x256x16`. Those are
different launch geometries of the same kernel. If you want the gate to cover the
exact shape the miner will run, set `PROD_WG=256 PROD_UNIT=16` in Cell 2 and
expect the CPU oracle to take proportionally longer.
