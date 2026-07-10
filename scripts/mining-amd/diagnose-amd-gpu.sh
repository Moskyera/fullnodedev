#!/usr/bin/env bash
# AMD OpenCL diagnostic — Linux
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$REPO_ROOT/target/release"
[[ -x "$BIN/poworker" ]] || BIN="$REPO_ROOT/target/debug"

echo "=== Hacash AMD GPU diagnostic ==="
echo "Repo: $REPO_ROOT"

if [[ ! -x "$BIN/diagnose_opencl" ]]; then
  echo "Building..."
  (cd "$REPO_ROOT" && cargo build --release --features ocl)
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