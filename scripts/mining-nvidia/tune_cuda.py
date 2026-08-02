#!/usr/bin/env python3
# Extracted verbatim from the Cell 5b block in colab_cuda_pool_e2e.md so it can
# be run as a file instead of pasted. 643 lines is too many to copy into a
# notebook reliably, and a truncated paste would fail in ways that look like a
# tuner fault rather than a copy fault.
#
#   python3 scripts/mining-nvidia/tune_cuda.py
#
# The doc section around that block explains what PASS and FAIL look like and
# what has never run on an NVIDIA device.

# ===========================================================================
# Cell 5b: tune this card with the REAL tuner, and prove the tune was worth it
# ===========================================================================
#
# WHAT RUNS. app/src/autotune16.rs, reached the way an operator reaches it:
# [efficiency] benchmark_seconds > 0 in a poworker config with [gpu] use_cuda =
# true. poworker::run_cuda_benchmark builds the TuneRequest, the tuner probes the
# card, plans a shared corpus, proves EVERY candidate against x16rs::block_hash
# over its whole launch window, sweeps, refines, soaks until the card stops
# moving, and patches the ini it was given. Nothing here re-implements a sweep, a
# score, a proof or a pick. What this cell adds is the one thing the tuner cannot
# do for itself: an independent fixed-work measurement of the shape it chose
# against the shape it started from, and a refusal to let a tune that failed its
# own proofs reach the config the miner runs.
#
# TWO EXIT-CODE TRAPS, both already walked into on this project:
#
#   * %%bash swallows exit codes, and so does `!cmd`: IPython ignores the status,
#     so the next cell runs on whatever the last successful run left behind.
#     Every process below runs under subprocess and its exit code is PRINTED on
#     its own line and then read.
#   * poworker exits 0 EVEN WHEN THE TUNE WAS REFUSED. run_block_mining_benchmark
#     returns, poworker() returns, main() returns, status 0. "[autotune]
#     REJECTED", "the card never settled" and a clean win all exit 0 alike. The
#     exit code here is necessary and never sufficient: the verdict is parsed out
#     of the report, and this cell says which of the two it is reading.
#
# THE COST TRAP. A warmup measured in BATCHES rather than seconds cost this
# project 37 minutes of blank screen once. So: the estimate is printed BEFORE any
# work starts, the tuner's own estimate is echoed and projected the moment it
# appears, a heartbeat prints during silence, and a hard timeout kills the run
# rather than letting it eat the session.
#
# WHERE THE TIME GOES, and it is not where you would guess. On a free 2-vCPU
# Colab VM the CPU oracle runs on ONE thread (autotune_oracle_threads is
# available_parallelism() - 2, floored at 1) and it CPU-hashes every candidate's
# entire launch window at repeat 16. That is minutes per candidate and it dwarfs
# the GPU sweep. It is also the thing being bought: a shape whose hashes were
# never proved equal to the CPU's is a number, not a result.

import math, os, queue, re, shutil, subprocess, sys, threading, time

# ------------------------------------------------------------------ knobs --
D            = "/content/fullnodedev"
REL          = os.path.join(D, "target", "release")
TUNE_DIR     = "/content/tune"                            # the tuner's own config
MINER_CONFIG = os.path.join(REL, "poworker.config.ini")   # what Cell 7 mines with
CUDA_DEVICE  = 0
SIZE         = "default"   # "fast" | "default" | "full", see SIZES below
MODE         = "max"       # "max" ranks sustained hashrate, "eco" ranks kH/J
PRESET       = "nvidia_balanced"   # the shipped shape the tune is judged against
TIMEOUT_MIN  = 75          # hard kill on the tune. Nothing may outlive the session.
INSTALL      = True        # copy a PASSING tune into MINER_CONFIG

# The three sizes differ in ONE thing: [gpu] work_groups, which is the ceiling of
# the tuner's work-group axis (poworker.rs: max_wg = memory_wg.min(work_groups)).
# The floor is the card's multiprocessor count, so on a 40-SM T4 the axis is
# 48..cap on the dyadic grid, the coarse sweep takes the powers-of-two family of
# it, and the unit-size axis is 32/64/128 whatever the cap is.
#
# grid_nonces is the sum of those coarse candidates' launch windows on a 40-SM
# card, which is what the CPU oracle has to hash. It is an UPPER BOUND: the
# latency prune and the shared corpus only ever remove shapes.
SIZES = {
    #           wg ceiling      benchmark_seconds   sum of candidate windows
    "fast":    {"cap": 256, "seconds": 180, "grid_nonces": 25.7e6},
    "default": {"cap": 512, "seconds": 240, "grid_nonces": 55.1e6},
    "full":    {"cap": 768, "seconds": 360, "grid_nonces": 73.9e6},
}

# Constants the estimate is arithmetic on, each with its source.
T4_MHS          = 7.54e6   # measured on a real T4, repeat 16, at 256x256x64
ORACLE_HPS_CORE = 60_000.0 # x16rs_gate::CPU_ORACLE_HPS_PER_CORE
PROOF_LAUNCHES  = 33       # per candidate: 1 all-ones count, 31 rank thresholds,
                           # 1 best-hash reduction, each reading the whole window
SPREAD_PCT      = 2.6      # x16rs_gate::BETWEEN_PROCESS_SPREAD_PCT
BASELINE_RUNS   = 7
BASELINE_WARMUP = 4        # BATCHES, not seconds. Printed in both units below.
BASELINE_TARGET = 25e6     # nonces per baseline run, about 3.3 s on a T4
BASELINE_BUDGET = 20 * 60  # seconds allowed for both baselines together


def die(msg):
    """Stop the notebook loudly. A nonzero exit does not stop a Colab notebook,
    so everything here raises rather than trusting a status nobody reads."""
    lines = msg.strip().splitlines()
    print("\n" + "#" * 72)
    print("#  STOP")
    for line in lines:
        print("#  " + line)
    print("#" * 72)
    sys.stdout.flush()
    raise RuntimeError(lines[0])


def hhmm(seconds):
    seconds = int(max(0, seconds))
    return "%02d:%02d" % (seconds // 60, seconds % 60)


def wrap(text, width=66):
    out, line = [], ""
    for word in text.split():
        if len(line) + len(word) + 1 > width:
            out.append(line)
            line = word
        else:
            line = (line + " " + word).strip()
    if line:
        out.append(line)
    return out


# --------------------------------------------------------------- preflight --
if shutil.which("nvidia-smi") is None:
    die("""No nvidia-smi. This runtime has no NVIDIA GPU, so there is no card to
tune and every number below would be a measurement of nothing.
Runtime -> Change runtime type -> T4 GPU.""")

POWORKER = os.path.join(REL, "poworker")
GATE     = os.path.join(REL, "x16rs_gate")
for path, cell in ((POWORKER, "Cell 4"), (GATE, "Cell 2")):
    if not os.path.exists(path):
        die("%s is missing. Run %s first." % (path, cell))
if SIZE not in SIZES:
    die("SIZE must be one of: %s" % ", ".join(sorted(SIZES)))
size = SIZES[SIZE]

gpu_name = subprocess.run(
    ["nvidia-smi", "--query-gpu=name,power.limit,memory.total",
     "--format=csv,noheader"], capture_output=True, text=True).stdout.strip()
cpus = os.cpu_count() or 2
oracle_threads = max(1, cpus - 2)   # autotune_oracle_threads(), poworker.rs
is_t4 = "T4" in gpu_name.upper()

print("card            :", gpu_name)
print("vCPUs           : %d, so the tuner's CPU oracle gets %d thread(s)"
      % (cpus, oracle_threads))

# --------------------------------------------------- what this will cost ----
# All arithmetic on the two constants above, none of it measured on YOUR card.
# The tuner prints its own estimate within the first minute and that one IS
# measured; this is here so nobody stares at a blank cell until then.
est_oracle = size["grid_nonces"] / (ORACLE_HPS_CORE * oracle_threads)
est_launch = size["grid_nonces"] * PROOF_LAUNCHES / T4_MHS
est_sweep  = 0.8 * size["seconds"]                            # SWEEP_BUDGET_SHARE
est_soak   = min(900.0, max(90.0, size["seconds"] / 2.0))     # soak_cap_seconds
est_refine = 0.4 * (est_oracle + est_launch)                  # up to 8 neighbours
est_final  = 300.0                                            # 255-threshold proof
est_total  = est_oracle + est_launch + est_sweep + est_soak + est_refine + est_final

print("")
print("SIZE = %s: [gpu] work_groups = %d, [efficiency] benchmark_seconds = %d"
      % (SIZE, size["cap"], size["seconds"]))
print("estimate, at the 7.54 MH/s measured on a T4 and %d kH/s per oracle core:"
      % (ORACLE_HPS_CORE / 1000))
for label, value in (("CPU oracle, proves every candidate", est_oracle),
                     ("proof launches on the card", est_launch),
                     ("timed sweep passes", est_sweep),
                     ("refinement allowance", est_refine),
                     ("soak, at most", est_soak),
                     ("final 255-threshold proof, allowance", est_final)):
    print("    %-38s %5.0f s" % (label, value))
print("    %-38s %5.0f s  (about %d min)"
      % ("TOTAL, upper bound", est_total, round(est_total / 60)))
print("hard timeout    : %d min" % TIMEOUT_MIN)
if not is_t4:
    print("NOTE: this is not a T4. That estimate is grid arithmetic for a 40-")
    print("      multiprocessor card at 7.54 MH/s and does not transfer. Read the")
    print("      tuner's own '[autotune] estimated total' line instead.")
if MODE != "max":
    print("NOTE: MODE = %s, so the tuner ranks candidates on something other than"
          % MODE)
    print("      throughput. The fixed-work check at the end is a HASHRATE")
    print("      comparison, so a tuned shape that trades hashrate for watts is")
    print("      expected to lose it. Read the kH/J line above it.")
if est_total > TIMEOUT_MIN * 60:
    die("""The estimate (%d min) is longer than TIMEOUT_MIN (%d min), so this would
be killed part way and prove nothing. Raise TIMEOUT_MIN, or set SIZE = "fast",
which searches 48..256 work groups instead of 48..%d."""
    % (round(est_total / 60), TIMEOUT_MIN, size["cap"]))
sys.stdout.flush()


# ---------------------------------------------------- streaming subprocess --
def run_streaming(argv, cwd, deadline, label, watch=None):
    """Run argv, stamp every line with elapsed time, print a heartbeat while the
    child is quiet, kill it at `deadline`. `watch(line)` may return a string,
    which kills the child and becomes the abort reason.

    Returns (exit_code, lines, abort_reason); exit_code is None if it was killed.
    """
    print("\n>>> %s" % label)
    print(">>> " + " ".join(argv))
    sys.stdout.flush()
    proc = subprocess.Popen(argv, cwd=cwd, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, bufsize=1)
    lines, abort, q = [], None, queue.Queue()

    def reader():
        for line in proc.stdout:
            q.put(line.rstrip("\n"))
        q.put(None)

    threading.Thread(target=reader, daemon=True).start()
    started = last_seen = time.time()
    last_line = ""
    while True:
        try:
            line = q.get(timeout=5)
        except queue.Empty:
            line = ""
        if line is None:
            break
        if line != "":
            lines.append(line)
            last_seen, last_line = time.time(), line
            print("[+%s] %s" % (hhmm(time.time() - started), line))
            sys.stdout.flush()
            if watch is not None:
                abort = watch(line)
                if abort:
                    break
        elif time.time() - last_seen > 45:
            print("[+%s] ... still running, %ds since the last line. The CPU oracle"
                  " is silent while it hashes. Last line: %s"
                  % (hhmm(time.time() - started), int(time.time() - last_seen),
                     last_line[:80]))
            sys.stdout.flush()
            last_seen = time.time()
        if time.time() > deadline:
            abort = "the hard timeout"
            break
    if abort:
        print("\n[+%s] KILLING %s: %s" % (hhmm(time.time() - started), label, abort))
        proc.terminate()
        try:
            proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            proc.kill()
        print("exit code (%s): killed, no status" % label)
        sys.stdout.flush()
        return None, lines, abort
    rc = proc.wait()
    print("[+%s] %s finished" % (hhmm(time.time() - started), label))
    print("exit code (%s): %d" % (label, rc))
    sys.stdout.flush()
    return rc, lines, None


# --------------------------------- what shape does the SHIPPED preset give? --
# Read out of the binary rather than copied out of nvidia_launch.rs, so this
# cannot quote a ladder the build does not contain. A config with gpu_profile set
# and NO work_groups / unit_size keys makes resolve_gpu_tuning fall back to the
# preset, and PoWorkConf::new prints what it resolved before anything else runs.
os.makedirs(os.path.join(TUNE_DIR, "preset"), exist_ok=True)
preset_cfg = os.path.join(TUNE_DIR, "preset", "poworker.config.ini")
open(preset_cfg, "w").write("""connect = 127.0.0.1:1
supervene = 0

[gpu]
use_opencl = false
use_cuda = false
gpu_profile = %s

[efficiency]
mode = %s
benchmark_seconds = 0
""" % (PRESET, MODE))

print("\n>>> asking this build what %s resolves to" % PRESET)
sys.stdout.flush()
proc = subprocess.Popen([POWORKER, preset_cfg], cwd=REL, stdout=subprocess.PIPE,
                        stderr=subprocess.STDOUT, text=True, bufsize=1)
killer = threading.Timer(60, proc.kill)   # it must not be able to hang the cell
killer.start()
RE_EFF = re.compile(
    r"\[efficiency\] mode=(\S+) profile=(\S+) work_groups=(\d+) unit_size=(\d+)")
preset_line = None
try:
    for line in proc.stdout:
        line = line.rstrip("\n")
        print("   ", line)
        preset_line = RE_EFF.search(line)
        if preset_line:
            break
finally:
    killer.cancel()
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
if preset_line is None:
    die("""poworker never printed its [efficiency] line, so the shipped preset for
%s could not be read out of this build, and there is nothing to judge a tune
against.""" % PRESET)
preset_shape = (int(preset_line.group(3)), int(preset_line.group(4)))
print("shipped preset  : %s = work_groups %d, unit_size %d, local_size 256"
      % (PRESET, preset_shape[0], preset_shape[1]))
if preset_shape[0] > size["cap"]:
    print("NOTE: the preset's %d work groups is ABOVE this SIZE's %d ceiling, so"
          % (preset_shape[0], size["cap"]))
    print("      the tuner cannot reach the preset's own shape. The comparison at")
    print("      the end is still valid, it just is not a search that contains it.")
sys.stdout.flush()


# ------------------------------------------------------- the tuner's config --
# Every key the tune writes back MUST already be present: apply_benchmark_pick
# REPLACES keys, it does not add them, so a missing unit_size line would mean a
# tune that silently keeps the old value. gpu_profile, work_groups, unit_size and
# benchmark_seconds are all here for that reason.
#
# supervene = 0 means no CPU assist threads, so Economics::cpu_watts is 0 and the
# watts in the report are the card's, straight from nvidia-smi.
os.makedirs(TUNE_DIR, exist_ok=True)
tune_cfg = os.path.join(TUNE_DIR, "poworker.config.ini")
open(tune_cfg, "w").write("""connect = 127.0.0.1:18082
supervene = 0
nonce_max = 4294967295
notice_wait = 3

[gpu]
use_opencl = false
use_cuda = true
cuda_device = %d
gpu_profile = %s
work_groups = %d
local_size = 256
unit_size = 64

[efficiency]
mode = %s
benchmark_seconds = %d
dynamic_supervene = false
oom_fallback = true
max_temp_c = 0
pause_if_unprofitable = false
power_cost_kwh = 0
hac_price = 0
stats_file = tune-stats.json
""" % (CUDA_DEVICE, PRESET, size["cap"], MODE, size["seconds"]))
print("\ntune config     :", tune_cfg)
print("miner config    : %s %s" % (MINER_CONFIG,
      "" if os.path.exists(MINER_CONFIG) else "(MISSING: Cell 5 writes it)"))
print("no node needed  : the tune finishes and returns before poworker ever")
print("                  contacts `connect`.")
sys.stdout.flush()


# ------------------------------------------------------------- run the tune --
RE_ESTIMATE = re.compile(r"estimated total before the soak: about (\d+)s")
started_at = time.time()
deadline = started_at + TIMEOUT_MIN * 60


def watch(line):
    """Early abort. The tuner's own estimate covers the timed sweep passes and
    the CPU oracle. It does NOT cover the 33 proof launches per candidate, the
    refinement, the soak or the 255-threshold final proof, so this projects it by
    1.5 and adds the soak cap and the final-proof allowance. Better to stop in
    the first minute than to be killed 60 minutes in with nothing to show."""
    m = RE_ESTIMATE.search(line)
    if not m:
        return None
    tuner_est = float(m.group(1))
    need = tuner_est * 1.5 + est_soak + est_final
    left = deadline - time.time()
    print("    >>> the tuner's OWN estimate is %ds. Projected through the final"
          " proof: %d min. Left before the hard timeout: %d min."
          % (tuner_est, round(need / 60), round(left / 60)))
    sys.stdout.flush()
    if need > left:
        return ("this tune projects to %d more minutes and only %d remain. Nothing"
                " has been wasted yet: set SIZE = \"fast\", or raise TIMEOUT_MIN."
                % (round(need / 60), round(left / 60)))
    return None


rc, log, abort = run_streaming(
    [POWORKER, tune_cfg], REL, deadline,
    "the tuner (poworker, benchmark_seconds=%d)" % size["seconds"], watch=watch)
text = "\n".join(log)

# --------------------------------------------------------------- read it ----
UNITS = {"H/s": 1.0, "kH/s": 1e3, "MH/s": 1e6}
chosen = re.search(
    r"chosen shape\s*:\s*work_groups=(\d+) local_size=(\d+) unit_size=(\d+)", text)
sust = re.search(
    r"sustained\s*:\s*([\d.]+) (MH/s|kH/s|H/s) raw, ([\d.]+) (MH/s|kH/s|H/s)", text)
lat = re.search(r"batch latency\s*:\s*p50 (\d+) ms, p95 (\d+) ms", text)
power = re.search(r"board power\s*:\s*([\d.]+) W (measured|estimated)", text)
soak = re.search(
    r"soak\s*:\s*(\d+) passes over (\d+)s, (settled|DID NOT SETTLE[^\n]*)", text)
applied = re.search(
    r"\[benchmark\] Applied gpu_profile=(\S+) \(work_groups=(\d+), unit_size=(\d+)\)",
    text)
rejects = [l for l in log if "REJECTED" in l and "[autotune] REJECTED:" not in l]
proof_bad = [l for l in rejects if "failed the equivalence proof" in l]
refused = [l for l in log if "[autotune] REJECTED:" in l]

fail = []
if abort:
    fail.append("the run was killed: %s." % abort)
elif rc != 0:
    fail.append("poworker exited %s. It exits 0 even when a tune is refused, so a"
                " nonzero status is something else: a panic, or the VM's OOM"
                " killer." % rc)
if refused:
    fail.append("the tuner refused the whole session. %s" % refused[0])
if proof_bad:
    fail.append("%d candidate(s) FAILED THE CPU ORACLE, or errored inside the"
                " proof that runs it. A shape whose hashes were not proved equal"
                " to x16rs::block_hash must not reach a mining config, so nothing"
                " was installed. The first one: %s" % (len(proof_bad), proof_bad[0]))
elif rejects:
    fail.append("%d candidate(s) were rejected for reasons other than the proof."
                " A healthy tune rejects none, so this cell will not install a"
                " config over it." % len(rejects))
if chosen is None or sust is None:
    fail.append("no report block was printed, so no shape was chosen.")
if soak is not None and not soak.group(3).startswith("settled"):
    fail.append("the soak did not settle, so the shape is not proven to sustain:"
                " %s" % soak.group(3))
if applied is None and not fail:
    fail.append("the tuner never printed '[benchmark] Applied ...', so it reported"
                " a winner and did not patch its own config.")

print("\n" + "=" * 72)
print(" THE TUNE")
print("=" * 72)
for line in rejects + refused:
    print("  rejection:", line)
shape = raw = valid = None
if chosen and sust:
    shape = (int(chosen.group(1)), int(chosen.group(3)))
    raw = float(sust.group(1)) * UNITS[sust.group(2)]
    valid = float(sust.group(3)) * UNITS[sust.group(4)]
    print("  chosen shape    : work_groups=%d local_size=256 unit_size=%d"
          % (shape[0], shape[1]))
    print("  hashrate        : %.2f MH/s raw, %.2f MH/s after the stale work a"
          " template change throws away" % (raw / 1e6, valid / 1e6))
    if lat:
        print("  batch latency   : p50 %s ms, p95 %s ms, against the 1500 ms ceiling"
              % (lat.group(1), lat.group(2)))
    if power and power.group(2) == "measured":
        watts = float(power.group(1))
        print("  board power     : %.0f W, measured by nvidia-smi" % watts)
        print("  efficiency      : %.1f kH/J raw, %.1f kH/J after stale work."
              % (raw / watts / 1e3, valid / watts / 1e3))
        print("                    Card only: supervene = 0 here, so the tuner's")
        print("                    cpu_watts term is 0 and this is the whole draw")
        print("                    it scored on.")
    else:
        print("  board power     : NOT MEASURED, so there is no kH/J. On NVIDIA")
        print("                    that means nvidia-smi did not report power.draw,")
        print("                    and an eco tune would have ranked every shape on")
        print("                    one constant, which is max mode by another name.")
    if soak:
        print("  soak            : %s passes over %ss, %s"
              % (soak.group(1), soak.group(2), soak.group(3)))
else:
    print("  no report. The last lines the tuner printed were:")
    for line in log[-15:]:
        print("      " + line)
sys.stdout.flush()


# ------------------- an independent fixed-work check: tuned vs the preset ----
# Two `x16rs_gate baseline` runs: the same kernel, the same height (repeat 16) and
# the same fixed corpus the tuner used, measured by a DIFFERENT binary in a
# different process, so the tune does not mark its own homework.
#
# Identical work on both sides on purpose: --headers 1 pins both shapes to one
# intro, and the batch counts are chosen so both hash exactly the same nonce
# range. What is left is the between-process spread, about 2.6% on this kernel,
# and that is the bar a claimed gain has to clear.
compare = None
if shape and not fail:
    if shape == preset_shape:
        print("\nThe tuner chose the shipped preset's own shape, so there is nothing")
        print("to compare: the baseline would be the same shape twice.")
    else:
        per_w = shape[0] * 256 * shape[1]
        per_p = preset_shape[0] * 256 * preset_shape[1]
        block = (per_w * per_p) // math.gcd(per_w, per_p)   # identical work needs
        total = block * max(1, math.ceil(BASELINE_TARGET / block))  # a common multiple
        rate = raw if raw and raw > 0 else T4_MHS
        runs = BASELINE_RUNS
        while runs > 3 and 2 * runs * total / rate > BASELINE_BUDGET / 2:
            runs -= 1
        print("\nfixed-work check: %d runs of %d nonces on EACH shape, the same"
              % (runs, total))
        print("nonces and the same single header on both sides, about %.1f s a run"
              % (total / rate))
        print("at the tuned shape's own %.2f MH/s." % (rate / 1e6))
        print("warmup is %d BATCHES, once, before any timed run: about %.1f s for"
              % (BASELINE_WARMUP, BASELINE_WARMUP * per_w / rate))
        print("the tuned shape and %.1f s for the preset. Both sides together:"
              % (BASELINE_WARMUP * per_p / rate))
        print("about %d min." % max(1, round(2 * runs * total / rate / 60 + 1)))
        sys.stdout.flush()
        bl_deadline = time.time() + BASELINE_BUDGET

        def baseline(what, wg, us):
            rc2, out, ab = run_streaming(
                [GATE, "baseline", "--backend", "cuda",
                 "--cuda-device", str(CUDA_DEVICE),
                 "--work-groups", str(wg), "--local-size", "256",
                 "--unit-size", str(us), "--headers", "1",
                 "--batches", str(total // (wg * 256 * us)), "--runs", str(runs),
                 "--warmup", str(BASELINE_WARMUP)],
                REL, bl_deadline, "baseline %s (%dx256x%d)" % (what, wg, us))
            if ab or rc2 != 0:
                return None, None
            body = "\n".join(out)
            med = re.search(r"median\s*:\s*([\d.]+) (MH/s|kH/s|H/s)", body)
            spr = re.search(r"peak-to-peak ([\d.]+)%", body)
            if med is None:
                return None, None
            return (float(med.group(1)) * UNITS[med.group(2)],
                    float(spr.group(1)) if spr else None)

        tuned_hps, tuned_spread = baseline("tuned", shape[0], shape[1])
        preset_hps, preset_spread = baseline("preset", preset_shape[0], preset_shape[1])
        if tuned_hps is None or preset_hps is None:
            fail.append("the fixed-work baseline did not complete, so the tuned"
                        " shape was never compared with the shipped preset by"
                        " anything except the tuner itself.")
        else:
            delta = (tuned_hps - preset_hps) / preset_hps * 100.0
            compare = (tuned_hps, preset_hps, delta)
            print("\n" + "=" * 72)
            print(" TUNED vs SHIPPED PRESET: fixed work, separate processes")
            print("=" * 72)
            print("  tuned  %5dx256x%-4d: %6.2f MH/s   (its own runs spanned %s)"
                  % (shape[0], shape[1], tuned_hps / 1e6,
                     "%.2f%%" % tuned_spread if tuned_spread is not None else "?"))
            print("  preset %5dx256x%-4d: %6.2f MH/s   (its own runs spanned %s)"
                  % (preset_shape[0], preset_shape[1], preset_hps / 1e6,
                     "%.2f%%" % preset_spread if preset_spread is not None else "?"))
            print("  difference          : %+.2f%%, against the %.1f%% between-"
                  "process spread" % (delta, SPREAD_PCT))
            if abs(delta) < SPREAD_PCT:
                print("  VERDICT             : NO GAIN SHOWN. These two shapes measure")
                print("                        the same here. The tune is still worth")
                print("                        having, it PROVED the shape against the")
                print("                        CPU, but do not quote a speedup.")
            elif delta > 0:
                print("  VERDICT             : the tuned shape BEATS the shipped preset")
                print("                        by %.2f%%, which clears the spread." % delta)
            else:
                print("  VERDICT             : the tuned shape LOST to the preset by")
                print("                        %.2f%%, outside the spread. That is a"
                      % -delta)
                print("                        contradiction worth reporting, and no")
                print("                        config is installed over it.")
                fail.append("the tuned shape measured %.2f%% SLOWER than the shipped"
                            " preset in an independent fixed-work run." % -delta)
sys.stdout.flush()


# ------------------------------------------------- install, or say why not ---
def patch_ini(path, wg, us, profile):
    """Rewrite [gpu] work_groups / unit_size / gpu_profile, ADDING the keys when
    they are absent. efficiency.rs apply_benchmark_pick only replaces keys that
    already exist, which is right for the tuner's own config (written above with
    all of them) and not enough for a config written by Cell 5."""
    want = {"gpu_profile": str(profile), "work_groups": str(wg), "unit_size": str(us)}
    out, in_gpu, seen, gpu_at = [], False, set(), None
    for line in open(path).read().splitlines():
        t = line.strip()
        if t.startswith("["):
            in_gpu = t.lower() == "[gpu]"
            if in_gpu:
                gpu_at = len(out) + 1        # the line just after the header
        elif in_gpu and "=" in t and not t.startswith(("#", ";")):
            key = t.split("=", 1)[0].strip()
            if key in want:
                seen.add(key)
                line = "%s = %s" % (key, want[key])
        out.append(line)
    missing = ["%s = %s" % (k, v) for k, v in want.items() if k not in seen]
    if gpu_at is None:
        out += ["", "[gpu]"] + missing
    else:
        out[gpu_at:gpu_at] = missing
    open(path, "w").write("\n".join(out) + "\n")


print("\n" + "#" * 72)
if fail:
    print("#  RESULT: FAIL. Nothing was written to the miner's config.")
    for reason in fail:
        print("#")
        for line in wrap(reason):
            print("#  " + line)
    print("#")
    for line in wrap("The tuner may still have patched its OWN config at %s. That"
                     " file is not what Cell 7 mines with, and this cell copied"
                     " nothing out of it." % tune_cfg):
        print("#  " + line)
    print("#" * 72)
    print("total wall time : %s" % hhmm(time.time() - started_at))
    raise RuntimeError(fail[0])

print("#  RESULT: PASS")
print("#  shape %dx256x%d, proved against the CPU over its whole %d-nonce window,"
      % (shape[0], shape[1], shape[0] * 256 * shape[1]))
print("#  settled under soak, and %s"
      % ("measured %+.2f%% against the shipped preset." % compare[2] if compare
         else "identical to the shipped preset."))
print("#" * 72)
print("the tuner patched its own config: %s -> gpu_profile=%s work_groups=%s"
      " unit_size=%s" % (tune_cfg, applied.group(1), applied.group(2),
                         applied.group(3)))
if (int(applied.group(2)), int(applied.group(3))) != shape:
    die("""The shape in the report and the shape written to the ini disagree.
The report says %dx%d, the ini was given %sx%s. Do not mine on either until that
is understood.""" % (shape[0], shape[1], applied.group(2), applied.group(3)))
# And read it back off the disk, because "Applied" is a log line and the file is
# the thing. apply_benchmark_pick REPLACES keys and never adds them, so a config
# missing a key would print exactly this line and change nothing.
on_disk = dict(re.findall(r"^\s*(work_groups|unit_size)\s*=\s*(\d+)\s*$",
                          open(tune_cfg).read(), re.M))
if (int(on_disk.get("work_groups", -1)), int(on_disk.get("unit_size", -1))) != shape:
    die("""The tuner said it applied %dx%d but %s holds work_groups=%s
unit_size=%s. Nothing was installed."""
    % (shape[0], shape[1], tune_cfg, on_disk.get("work_groups"),
       on_disk.get("unit_size")))
if INSTALL and os.path.exists(MINER_CONFIG):
    patch_ini(MINER_CONFIG, shape[0], shape[1], applied.group(1))
    print("installed into %s:" % MINER_CONFIG)
    for line in open(MINER_CONFIG).read().splitlines():
        if line.strip().startswith(("work_groups", "unit_size", "gpu_profile")):
            print("    " + line)
elif INSTALL:
    print("MINER_CONFIG does not exist yet (Cell 5 writes it). Run this cell again")
    print("after Cell 5, or set [gpu] work_groups = %d and unit_size = %d there by"
          % (shape[0], shape[1]))
    print("hand.")
print("total wall time : %s" % hhmm(time.time() - started_at))
