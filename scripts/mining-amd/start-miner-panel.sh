#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

RUN_DIR="$REPO_ROOT/target/release"
[[ -x "$RUN_DIR/miner-panel" ]] || RUN_DIR="$REPO_ROOT/target/debug"

if [[ ! -x "$RUN_DIR/miner-panel" ]]; then
  echo "miner-panel not found. Run scripts/mining-amd/build-miner-panel.sh first."
  exit 1
fi

cd "$RUN_DIR"
exec ./miner-panel