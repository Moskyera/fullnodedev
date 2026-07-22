#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

required=(miner-panel poworker diaworker diagnose_opencl)
missing=0
for binary in "${required[@]}"; do
  if [[ ! -f "$SCRIPT_DIR/$binary" ]]; then
    echo "[MISSING] $binary"
    missing=1
  else
    chmod u+x "$SCRIPT_DIR/$binary"
  fi
done
if [[ ! -f "$SCRIPT_DIR/x16rs/opencl/x16rs_main.cl" ]]; then
  echo "[MISSING] x16rs/opencl/x16rs_main.cl"
  missing=1
fi
if (( missing )); then
  echo "This miner package is incomplete. Extract the complete Linux release."
  exit 1
fi

if [[ ! -f poworker.config.ini || ! -f diaworker.config.ini ]]; then
  HACASH_SETUP_NO_LAUNCH=1 "$SCRIPT_DIR/SETUP-LINUX.sh" --no-launch
fi

exec "$SCRIPT_DIR/miner-panel"
