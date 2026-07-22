#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

RUN_DIR="$REPO_ROOT/target/release"
[[ -x "$RUN_DIR/diaworker" ]] || RUN_DIR="$REPO_ROOT/target/debug"

if [[ ! -x "$RUN_DIR/diaworker" ]]; then
  echo "diaworker not found. Run build-amd-miner.sh first."
  exit 1
fi

if [[ ! -f "$RUN_DIR/diaworker.config.ini" ]]; then
  echo "Missing diaworker.config.ini — running install-configs.sh ..."
  "$SCRIPT_DIR/install-configs.sh"
fi

echo "Starting HACD CPU diamond miner from $RUN_DIR"
echo "HACD does not use OpenCL; supervene controls CPU threads."
echo "Requires fullnode with [diamondminer] enable = true"
echo
cd "$RUN_DIR"
exec ./diaworker