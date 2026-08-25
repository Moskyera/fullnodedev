#!/usr/bin/env bash
# Byte-equivalence gate for the CUDA kernels, on a real NVIDIA GPU (Colab T4 or
# any Linux NVIDIA box).
#
# This is the thing to run BEFORE believing any CUDA hashrate. It proves the
# card's hashes equal x16rs::block_hash byte for byte, at repeat 1/4/8/16, with
# EVERY hash in each window compared. Then, unless SKIP_FAULTS=1, it proves the
# gate can FAIL, by rebuilding the CUDA kernels against three trees with
# deliberate defects in them and requiring the gate to catch them.
#
# A gate that has only ever been seen to pass proves nothing about the kernel;
# it proves the gate is quiet.
#
#   bash scripts/mining-nvidia/colab_cuda_gate.sh
#
# Exit 0 = the card agrees with the CPU AND the gate demonstrably catches
# defects. Anything else is a failure a script can see.
#
# RESUMABLE. Every step that costs minutes writes a marker under
# target/gate-state/<fingerprint>/ when it succeeds, and a re-run skips it. The
# fingerprint covers the git commit, the kernel sources and the gate sources, so
# touching any of them starts over by itself. A Colab T4 session that dies half
# way through therefore costs you the step it was on, not the whole run.
# RESUME=0 forces everything to be redone.
#
# Env:
#   SKIP_FAULTS=1     clean run only (about 4x faster, and proves 4x less)
#   ALLOW_RACE_MISS=1 let fault B (a deleted barrier, i.e. a data race) go
#                     uncaught without failing the run. Read the note at step 2
#                     before using this: it downgrades the result string, on
#                     purpose, so a run that used it can never be quoted as a
#                     clean PASS.
#   RESUME=0          ignore previous markers and redo every step
#   PROD_WG           production-shape work_groups (default 48)
#   PROD_UNIT         production-shape unit_size   (default 48)
#   PROD_BATCHES      production windows (default 1; each costs a full CPU
#                     oracle over work_groups*256*unit_size nonces)
#   HEADERS           corpus headers (default 4)

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

SKIP_FAULTS="${SKIP_FAULTS:-0}"
ALLOW_RACE_MISS="${ALLOW_RACE_MISS:-0}"
RESUME="${RESUME:-1}"
PROD_WG="${PROD_WG:-48}"
PROD_UNIT="${PROD_UNIT:-48}"
PROD_BATCHES="${PROD_BATCHES:-1}"
HEADERS="${HEADERS:-4}"

# Faults that are races rather than wrong arithmetic. A race is caught because
# the hardware happened to interleave badly, so "not caught" on some card is a
# statement about that card's scheduler, not about the gate's arithmetic. Kept
# as a list so the reason travels with the exception.
RACE_FAULTS=" B "

# Colab free tier dies on workspace LTO. None of these change what is computed.
export CARGO_TERM_COLOR=always
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_RELEASE_LTO=false
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
export CARGO_PROFILE_RELEASE_OPT_LEVEL=2
export CARGO_PROFILE_RELEASE_STRIP=false

LOG_DIR="${ROOT}/scripts/mining-nvidia/colab-results"
mkdir -p "$LOG_DIR"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
LOG="${LOG_DIR}/gate-${STAMP}.log"
SUMMARY="${LOG_DIR}/latest-gate-summary.txt"
TREES="${ROOT}/target/gate-trees"

exec > >(tee -a "$LOG") 2>&1

T0=$(date +%s)
elapsed() {
  local s=$(( $(date +%s) - T0 ))
  printf '%02d:%02d' $(( s / 60 )) $(( s % 60 ))
}
step() {
  echo ""
  echo "----------------------------------------------------------------------"
  echo "[+$(elapsed)] $*"
  echo "----------------------------------------------------------------------"
}

echo "=============================================="
echo " Hacash CUDA byte-equivalence gate"
echo " time (UTC): ${STAMP}"
echo " log:        ${LOG}"
echo "=============================================="

# ---------------------------------------------------------------------------
# Which code is being gated. Printed before anything else, because a gate run
# whose commit nobody wrote down is a number without a subject.
# ---------------------------------------------------------------------------
COMMIT="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo 'not-a-git-checkout')"
DIRTY="$(git -C "$ROOT" status --porcelain 2>/dev/null | head -c 1)"
echo "commit:  ${COMMIT}$( [[ -n "$DIRTY" ]] && echo ' (working tree DIRTY)' )"

# The fingerprint keys the resume markers. It has to move when anything that
# could change the verdict moves, and stay still otherwise.
fingerprint() {
  {
    echo "$COMMIT"
    cat "$ROOT"/x16rs/opencl/*.cl \
        "$ROOT"/x16rs-cuda/cuda/*.cu \
        "$ROOT"/x16rs-cuda/cuda/*.cuh \
        "$ROOT"/x16rs-cuda/build.rs \
        "$ROOT"/x16rs-cuda/src/lib.rs \
        "$ROOT"/app/src/x16rs_gate.rs \
        "$ROOT"/src/bin/x16rs_gate.rs 2>/dev/null
    echo "shape=${PROD_WG}x256x${PROD_UNIT}x${PROD_BATCHES} headers=${HEADERS}"
  } | sha256sum | cut -c1-16
}
FP="$(fingerprint)"
STATE="${ROOT}/target/gate-state/${FP}"
if [[ "$RESUME" != "1" ]]; then
  rm -rf "$STATE"
fi
mkdir -p "$STATE"
echo "state:   ${STATE}"
echo "         (delete it, or set RESUME=0, to redo steps this fingerprint already proved)"

fail() {
  echo ""
  echo "######################################################################"
  echo "#  RESULT: FAIL"
  echo "#  $1"
  echo "######################################################################"
  if [[ -n "${X16RS_CUDA_KERNEL_DIR:-}" ]]; then
    echo ""
    echo "WARNING: this run stopped while ${GATE} was built from ${X16RS_CUDA_KERNEL_DIR},"
    echo "         a kernel tree that is WRONG ON PURPOSE. Rebuild before using that binary:"
    echo "           unset X16RS_CUDA_KERNEL_DIR"
    echo "           cargo build --release --features cuda --bin x16rs_gate"
  fi
  {
    echo "result=FAIL"
    echo "reason=$1"
    echo "commit=${COMMIT}"
    echo "stamp=${STAMP}"
    echo "log=${LOG}"
  } > "$SUMMARY"
  exit 1
}

step "0/3  host, GPU and toolkit"
nvidia-smi || fail "no nvidia-smi: this box has no NVIDIA GPU, or the Colab runtime is not set to GPU (Runtime -> Change runtime type -> T4 GPU)"
nvcc --version || fail "no nvcc: the CUDA toolkit is not on PATH"

# The gate needs the CUDA kernels compiled in. build.rs only compiles them when
# it finds nvcc, and quietly builds a kernel-less crate when it does not, so
# point it at the toolkit explicitly rather than hope.
if [[ -z "${CUDA_PATH:-}" && -d /usr/local/cuda ]]; then
  export CUDA_PATH=/usr/local/cuda
fi
echo "CUDA_PATH=${CUDA_PATH:-<unset>}"
nproc 2>/dev/null | sed 's/^/CPU cores (the gate hashes the oracle on these): /'

GATE="${ROOT}/target/release/x16rs_gate"

build_gate() {
  local label="$1"
  echo ""
  echo ">>> [+$(elapsed)] building the gate (${label})"
  # Heartbeat: free Colab looks dead during a long compile, and a silent cell is
  # what makes an operator interrupt a build that was going to finish.
  ( while true; do sleep 60; echo "    ... [+$(elapsed)] still compiling (${label})"; done ) &
  local hb=$!
  cargo build --release --features cuda --bin x16rs_gate
  local rc=$?
  kill "$hb" 2>/dev/null; wait "$hb" 2>/dev/null
  echo ">>> [+$(elapsed)] build (${label}) exit=${rc}"
  return $rc
}

# ---------------------------------------------------------------------------
# 1. The real kernels must agree with the CPU.
# ---------------------------------------------------------------------------
step "1/3  equivalence: do the shipping CUDA kernels equal x16rs::block_hash?"
if [[ -f "${STATE}/clean.ok" ]]; then
  echo "ALREADY PROVED for this fingerprint at $(cat "${STATE}/clean.ok"); skipping."
  echo "(RESUME=0 to redo it.)"
else
  unset X16RS_CUDA_KERNEL_DIR
  build_gate "shipping kernels" || fail "the CUDA build failed"

  "$GATE" equiv --backend cuda \
    --headers "$HEADERS" \
    --prod-batches "$PROD_BATCHES" \
    --work-groups "$PROD_WG" --local-size 256 --unit-size "$PROD_UNIT"
  clean_rc=$?
  case $clean_rc in
    0) : ;;
    3) fail "the shipping CUDA kernels do NOT match the CPU. Do not mine with this build and \
do not report a hashrate from it. The report above names the mismatching nonces, and, when the \
evidence allows it, the algorithm." ;;
    # Exit 4 now means only what it says. It used to also carry the gate's own
    # detections: the production count threshold, the best-hash reduction, and a
    # bad window dump all reported a broken kernel by returning Err, and every
    # Err became exit 4. So a run where the gate had just caught the fault it
    # exists to catch printed "nothing was compared" here. Those messages are
    # tagged at the source now and exit 3 with the rest of the mismatches.
    4) fail "the gate could not open or run the CUDA device (exit 4). The message above is the \
CUDA runtime's own. Nothing was compared." ;;
    1) fail "the gate refused to run (exit 1): it was built without the cuda feature, or without \
kernels because nvcc was not found at build time. Nothing was compared." ;;
    *) fail "the gate exited ${clean_rc}, which is not a verdict. Nothing was compared." ;;
  esac
  echo "PASS: the card's hashes are the CPU's, byte for byte."
  date -u +%Y%m%dT%H%M%SZ > "${STATE}/clean.ok"
fi

if [[ "$SKIP_FAULTS" == "1" ]]; then
  step "2/3  SKIPPED (SKIP_FAULTS=1)"
  echo "The gate was NOT shown to be able to fail on this box."
  echo "The equivalence result above still stands; what is unproven is the gate itself."
  {
    echo "result=PASS-UNPROVEN"
    echo "note=clean run only; the gate was never shown to catch a defect here"
    echo "commit=${COMMIT}"
    echo "stamp=${STAMP}"
    echo "log=${LOG}"
  } > "$SUMMARY"
  exit 0
fi

# ---------------------------------------------------------------------------
# 2. And the gate must catch defects, or step 1 meant nothing.
#
# block_miner.cu includes the algorithm sources from x16rs/opencl, so the three
# trees that proved the OpenCL gate prove this one: A puts shabal's counter off
# by one, B deletes a barrier, C flips one bit of blake's IV. A and C are single
# algorithms and the gate is expected to NAME them; B is a race and the honest
# answer is that no single algorithm is implicated.
#
# A and C are arithmetic: the kernel computes a different number, every time, on
# every device. If either goes uncaught the gate is broken and this run fails.
#
# B is a data race. It was caught on an AMD gfx1201. Whether a given NVIDIA
# scheduler interleaves badly enough to produce a wrong hash in 12288 nonces is
# a property of that card, not of the gate. It has NOT been observed on an
# NVIDIA device by anyone here. So a B that goes uncaught is treated as a
# failure by default (it might really be a gate defect) but ALLOW_RACE_MISS=1
# downgrades it to a named, recorded weaker result rather than a stop.
# ---------------------------------------------------------------------------
step "2/3  fault injection: can this gate FAIL on kernels that are wrong on purpose?"
python3 scripts/x16rs_gate_trees.py x16rs/opencl "$TREES" faults \
  || fail "could not build the fault trees"

caught=0
missed_race=""
for f in A B C; do
  echo ""
  echo "--- [+$(elapsed)] fault ${f} ---"
  if [[ -f "${STATE}/fault-${f}.ok" ]]; then
    echo "ALREADY CAUGHT for this fingerprint at $(cat "${STATE}/fault-${f}.ok"); skipping."
    caught=$((caught + 1))
    continue
  fi
  if [[ -f "${STATE}/fault-${f}.race-miss" ]]; then
    echo "ALREADY RECORDED as not reproduced on this card at $(cat "${STATE}/fault-${f}.race-miss")."
    missed_race="${missed_race}${f}"
    continue
  fi
  export X16RS_CUDA_KERNEL_DIR="${TREES}/faults/${f}"
  build_gate "fault ${f}" || fail "the CUDA build failed for fault tree ${f}"
  # No production pass here: one exhaustive window already fails thousands of
  # nonces, and the point is the verdict, not the volume.
  "$GATE" equiv --backend cuda --headers 1 --batches 1 --prod-batches 0
  rc=$?
  if [[ $rc -eq 3 ]]; then
    echo "OK: fault ${f} was caught (exit 3)."
    caught=$((caught + 1))
    date -u +%Y%m%dT%H%M%SZ > "${STATE}/fault-${f}.ok"
  elif [[ "$RACE_FAULTS" == *" ${f} "* && "$ALLOW_RACE_MISS" == "1" ]]; then
    echo "NOT CAUGHT: fault ${f} is a data race and this card did not reproduce it"
    echo "            (gate exit ${rc}). ALLOW_RACE_MISS=1, so the run continues with a"
    echo "            WEAKER result string. Do not quote this run as a clean PASS."
    missed_race="${missed_race}${f}"
    date -u +%Y%m%dT%H%M%SZ > "${STATE}/fault-${f}.race-miss"
  else
    extra=""
    if [[ "$RACE_FAULTS" == *" ${f} "* ]]; then
      extra=" Fault ${f} is a deleted barrier, i.e. a data race, and a race can fail to \
manifest on a given scheduler. If faults A and C were both caught, the gate's arithmetic is \
working and this is most likely this card rather than the gate; re-run with ALLOW_RACE_MISS=1 to \
continue with a result string that records exactly that."
    fi
    unset X16RS_CUDA_KERNEL_DIR
    fail "fault ${f} was NOT caught (gate exit ${rc}, expected 3). The gate cannot detect a \
broken kernel, so its PASS in step 1 means nothing.${extra}"
  fi
done
unset X16RS_CUDA_KERNEL_DIR

# Leave the operator with a binary that has the REAL kernels in it. A fault-tree
# build sitting at target/release/x16rs_gate is a loaded gun, and after a
# resumed run nobody can tell by looking which tree it came from. Cargo makes
# this a no-op when the last build was already the shipping one.
step "3/3  restoring the shipping kernels in target/release/x16rs_gate"
build_gate "shipping kernels (restore)" || fail "could not rebuild the shipping kernels"

echo ""
if [[ -n "$missed_race" ]]; then
  RESULT="PASS-RACE-NOT-REPRODUCED"
else
  RESULT="PASS"
fi
echo "=============================================="
echo " RESULT: ${RESULT}   (wall +$(elapsed))"
echo "  - the CUDA kernels match x16rs::block_hash byte for byte"
echo "  - the gate caught ${caught}/3 deliberately broken kernels"
if [[ -n "$missed_race" ]]; then
echo "  - fault(s) ${missed_race} (data race) did NOT reproduce on this card, and were"
echo "    waived by ALLOW_RACE_MISS=1. The arithmetic faults were caught; the race was"
echo "    not exercised. This is weaker than a clean PASS and is recorded as such."
fi
echo "=============================================="
{
  echo "result=${RESULT}"
  echo "faults_caught=${caught}/3"
  [[ -n "$missed_race" ]] && echo "race_not_reproduced=${missed_race}"
  echo "prod_shape=${PROD_WG}x256x${PROD_UNIT} x ${PROD_BATCHES}"
  echo "commit=${COMMIT}"
  echo "stamp=${STAMP}"
  echo "log=${LOG}"
} > "$SUMMARY"
exit 0
