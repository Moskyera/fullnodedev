#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

for profile in debug release; do
  OUT="$REPO_ROOT/target/$profile"
  if [[ -d "$OUT" ]]; then
    cp -f "$SCRIPT_DIR/poworker.amd.ini.example" "$OUT/poworker.config.ini"
    cp -f "$SCRIPT_DIR/diaworker.amd.ini.example" "$OUT/diaworker.config.ini"
    echo "Installed configs in $OUT"
  fi
done

if [[ ! -f "$REPO_ROOT/target/release/poworker" && ! -f "$REPO_ROOT/target/debug/poworker" ]]; then
  echo "poworker not found — run build-amd-miner.sh first."
  exit 1
fi

echo
echo "  Edit platform_id / device_ids after list-opencl-devices.sh"
echo "  Fullnode example: copy hacash.config.ini to target/release/ if needed"
echo