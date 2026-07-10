#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

for profile in release debug; do
  BIN="$REPO_ROOT/target/$profile/list_opencl"
  if [[ -x "$BIN" ]]; then
    exec "$BIN"
  fi
done

echo "list_opencl not found. Run scripts/mining-amd/build-amd-miner.sh first."
exit 1