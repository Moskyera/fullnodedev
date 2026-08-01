#!/usr/bin/env bash
# CUDA crate smoke for Google Colab FREE TIER (T4) or any Linux NVIDIA box.
#
# THIS IS NOT THE EQUIVALENCE GATE. It runs the x16rs-cuda test suite: the
# genesis vector, the differential tests over a few thousand inputs, and the pool
# share list's bookkeeping (overflow, counter isolation, readback bounds). That
# is real coverage and it is worth having, but the claim "every hash this card
# computes equals x16rs::block_hash" comes from
#
#   bash scripts/mining-nvidia/colab_cuda_gate.sh
#
# which compares whole windows byte for byte and is itself proved able to fail.
# Run the gate first; run this after it.
#
# FREE TIER MODE (default): tests only.
#   Skips full poworker --release (that can run for hours with workspace LTO).
#
# Full mode (optional, Pro / long session):
#   COLAB_FULL=1 bash scripts/mining-nvidia/colab_cuda_smoke.sh
#
# Exit 0 = PASS

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

# Free-tier defaults: fast profile overrides (ignore workspace LTO if anything uses release)
export CARGO_TERM_COLOR=always
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_RELEASE_LTO=false
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
export CARGO_PROFILE_RELEASE_OPT_LEVEL=2
export CARGO_PROFILE_RELEASE_STRIP=false
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_DEV_OPT_LEVEL=1

COLAB_FULL="${COLAB_FULL:-0}"
MODE="FREE-TIER (x16rs-cuda tests only)"
if [[ "$COLAB_FULL" == "1" ]]; then
  MODE="FULL (tests + poworker release, no LTO)"
fi

LOG_DIR="${ROOT}/scripts/mining-nvidia/colab-results"
mkdir -p "$LOG_DIR"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
LOG="${LOG_DIR}/smoke-${STAMP}.log"
SUMMARY="${LOG_DIR}/latest-summary.txt"

exec > >(tee -a "$LOG") 2>&1

echo "=============================================="
echo " Hacash CUDA smoke: ${MODE}"
echo " time (UTC): ${STAMP}"
echo " repo:       ${ROOT}"
echo " log:        ${LOG}"
echo "=============================================="
echo "TIP: Compiling lines are normal. Free tier: expect ~10-25 min for tests only."
echo "     If stuck >40 min on FREE, Runtime->Interrupt and re-run this script."
echo ""

pass=0
fail=0
POW_RC=0
TEST_RC=1

# Heartbeat while long cargo runs (free Colab looks "dead" without output)
run_with_heartbeat() {
  local label="$1"
  shift
  echo ""
  echo ">>> START: ${label}  ($(date -u +%H:%M:%S) UTC)"
  (
    while true; do
      sleep 60
      echo "    ... still working: ${label}  ($(date -u +%H:%M:%S) UTC)  (free Colab: keep tab open)"
    done
  ) &
  local hb=$!
  set +e
  "$@"
  local rc=$?
  set -e
  kill "$hb" 2>/dev/null || true
  wait "$hb" 2>/dev/null || true
  echo ">>> END: ${label}  exit=${rc}  ($(date -u +%H:%M:%S) UTC)"
  return $rc
}

# --- GPU ---
echo "=== Host / GPU ==="
uname -a || true
if ! command -v nvidia-smi >/dev/null 2>&1; then
  echo "ERROR: no GPU. Runtime -> Change runtime type -> T4 GPU, then re-run."
  exit 1
fi
nvidia-smi
GPU_NAME="$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1 || echo unknown)"
echo "GPU_NAME=${GPU_NAME}"

# --- CUDA ---
if [[ -z "${CUDA_PATH:-}" ]]; then
  if [[ -x /usr/local/cuda/bin/nvcc ]]; then
    export CUDA_PATH=/usr/local/cuda
  elif [[ -x /usr/local/cuda-12/bin/nvcc ]]; then
    export CUDA_PATH=/usr/local/cuda-12
  elif command -v nvcc >/dev/null 2>&1; then
    export CUDA_PATH="$(dirname "$(dirname "$(command -v nvcc)")")"
  fi
fi
export CUDA_HOME="${CUDA_HOME:-${CUDA_PATH:-}}"
export PATH="${CUDA_PATH:-/usr/local/cuda}/bin:${PATH}"
export LD_LIBRARY_PATH="${CUDA_PATH:-/usr/local/cuda}/lib64:${LD_LIBRARY_PATH:-}"

echo "CUDA_PATH=${CUDA_PATH:-unset}"
if ! command -v nvcc >/dev/null 2>&1; then
  echo "ERROR: nvcc not found"
  exit 1
fi
nvcc --version

# --- Rust ---
if ! command -v rustc >/dev/null 2>&1; then
  echo "Installing rustup..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
fi
# shellcheck disable=SC1091
[[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
rustc --version
cargo --version

if [[ -n "${CUDA_PATH:-}" ]]; then
  export CUDA_PATH CUDA_HOME
fi

# --- CRITICAL: only the CUDA crate (debug profile = much faster than release+LTO) ---
echo ""
echo "=== [required] cargo test -p x16rs-cuda --features cuda ==="
echo "    (debug build; NOT full miner; free-tier safe)"

# The guards below read the test output back. They must NOT read "$LOG": that
# file is written by the `exec > >(tee ...)` at the top, whose tee is a separate
# process, so a grep run microseconds after the test finishes can read a file
# that is still short. Capture the test's own output to its own file and grep
# that; the pipeline's status is cargo's because pipefail is set at the top.
TEST_LOG="${LOG_DIR}/tests-${STAMP}.log"
run_cuda_tests() {
  cargo test -p x16rs-cuda --features cuda -- --nocapture 2>&1 | tee "$TEST_LOG"
}
if run_with_heartbeat "x16rs-cuda-tests" run_cuda_tests; then
  TEST_RC=0
  echo "    PASS: x16rs-cuda tests"
  pass=$((pass + 1))
else
  TEST_RC=$?
  echo "    FAIL: x16rs-cuda tests (exit ${TEST_RC})"
  fail=$((fail + 1))
fi
[[ -f "$TEST_LOG" ]] || : > "$TEST_LOG"

# Guard 1: a test that SKIPS still counts as a pass. Every wording the suite
# uses to decline has to be caught, not just one of them.
for excuse in "CUDA kernels not compiled" "no usable CUDA device" "skipping"; do
  if grep -q "$excuse" "$TEST_LOG"; then
    echo "    FAIL: a GPU test declined to run (\"${excuse}\"), so this green result"
    echo "          covers nothing about the card."
    fail=$((fail + 1))
    TEST_RC=1
    break
  fi
done

# Guard 2: the worse case, where a test does not skip because it was never
# compiled. gpu_share_list_tests is #[cfg(all(test, cuda_available))] and
# cuda_available is set by build.rs ONLY when it found nvcc. Without nvcc the
# module vanishes and cargo prints a tidy "ok" for what is left, with no excuse
# to grep for. The only reliable check is that the test NAMES appear.
for want in \
  "cuda_matches_cpu_across_many_inputs" \
  "cuda_batch_matches_cpu" \
  "cuda_genesis_block_hash_when_available" \
  "the_share_list_matches_the_cpu_and_leaves_the_best_result_untouched" \
  "the_blake_iv_is_not_duplicated_into_the_cuda_source"; do
  if ! grep -q "$want" "$TEST_LOG"; then
    echo "    FAIL: ${want} never ran."
    echo "          Look for the cargo warning \"Using CUDA Toolkit at ...\" above. If it says"
    echo "          \"CUDA Toolkit not found\" instead, the GPU tests were never compiled."
    fail=$((fail + 1))
    TEST_RC=1
  fi
done

# --- OPTIONAL full poworker (skip on free tier) ---
if [[ "$COLAB_FULL" == "1" ]]; then
  echo ""
  echo "=== [optional FULL] poworker release (LTO forced OFF) ==="
  if run_with_heartbeat "poworker-release" \
    cargo build --release --bin poworker --features cuda; then
    POW_RC=0
    echo "    PASS: poworker"
    pass=$((pass + 1))
    ls -la target/release/poworker || true
  else
    POW_RC=$?
    echo "    FAIL: poworker (exit ${POW_RC})"
    fail=$((fail + 1))
  fi
else
  echo ""
  echo "=== SKIP poworker release (free tier default) ==="
  echo "    Phase 1 PASS does not need the full miner binary."
  echo "    Later: COLAB_FULL=1 bash scripts/mining-nvidia/colab_cuda_smoke.sh"
  POW_RC=0
fi

echo ""
echo "=============================================="
echo " SUMMARY  mode=${MODE}"
echo "=============================================="
echo "GPU:  ${GPU_NAME}"
echo "PASS: ${pass}"
echo "FAIL: ${fail}"
echo "Log:  ${LOG}"

if [[ $fail -eq 0 && $TEST_RC -eq 0 ]]; then
  RESULT=PASS
else
  RESULT=FAIL
fi

{
  echo "stamp=${STAMP}"
  echo "mode=${MODE}"
  echo "gpu=${GPU_NAME}"
  echo "pass=${pass}"
  echo "fail=${fail}"
  echo "test_rc=${TEST_RC}"
  echo "pow_rc=${POW_RC}"
  echo "log=${LOG}"
  echo "result=${RESULT}"
} | tee "$SUMMARY"

if [[ "$RESULT" == "PASS" ]]; then
  echo ""
  echo "OVERALL: PASS (CUDA kernels validated on this GPU)"
  echo "Download: scripts/mining-nvidia/colab-results/"
  echo "No push required yet; keep the logs."
  exit 0
fi

echo ""
echo "OVERALL: FAIL"
exit 1
