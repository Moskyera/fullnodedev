#!/usr/bin/env bash
# AMD OpenCL diagnostic — Linux
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

find_bin_dir() {
  local profile
  for profile in release debug; do
    if [[ -x "$REPO_ROOT/target/$profile/diagnose_opencl" ]]; then
      printf '%s\n' "$REPO_ROOT/target/$profile"
      return 0
    fi
  done
  return 1
}

echo "=== Hacash AMD GPU diagnostic ==="
echo "Repo: $REPO_ROOT"

BIN="$(find_bin_dir || true)"
if [[ -z "$BIN" ]]; then
  echo "Building..."
  (cd "$REPO_ROOT" && cargo build --locked --release --features ocl --bin diagnose_opencl --bin poworker)
  BIN="$(find_bin_dir || true)"
fi
if [[ -z "$BIN" ]]; then
  echo "diagnose_opencl was not produced by the release build." >&2
  exit 1
fi

echo "--- OpenCL scan ---"
"$BIN/diagnose_opencl" --report "$BIN/diagnose-opencl.json"

CFG="$BIN/poworker.config.ini"
if [[ -f "$CFG" ]]; then
  echo "--- Pure-GPU benchmark (45s) ---"
  cp "$CFG" "$CFG.diagbak"
  sed -i 's/^cpu_assist = .*/cpu_assist = false/' "$CFG" 2>/dev/null || true
  grep -q '^benchmark_seconds' "$CFG" && sed -i 's/^benchmark_seconds = .*/benchmark_seconds = 45/' "$CFG" || echo 'benchmark_seconds = 45' >> "$CFG"
  (cd "$BIN" && ./poworker) 2>&1 | tee "$BIN/diagnose-benchmark.log" || true
  mv "$CFG.diagbak" "$CFG"
fi

echo "Reports: $BIN/diagnose-opencl.json"