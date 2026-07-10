#!/usr/bin/env bash
# Build miner-panel GUI (eframe) on Linux — optional; miners work without it.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

echo
echo "  Building miner-panel (GUI)..."
echo

if command -v apt-get >/dev/null 2>&1; then
  MISSING=
  for pkg in libxcb-render0 libxkbcommon0; do
    if ! dpkg -s "$pkg" >/dev/null 2>&1; then
      MISSING=1
    fi
  done
  if [[ -n "${MISSING:-}" ]]; then
    echo "  GUI runtime libs may be missing. Install with:"
    echo "    sudo apt install libxcb-render0 libxcb-shape0 libxkbcommon0 libgtk-3-0"
    echo "  Build deps (if compile fails):"
    echo "    sudo apt install libgtk-3-dev libxcb-render0-dev libxkbcommon-dev libssl-dev"
    echo
  fi
fi

cargo build --release -p miner-panel

echo
echo "  OK: $REPO_ROOT/target/release/miner-panel"
echo "  Run from same folder as poworker, hacash, list_opencl:"
echo "    cd target/release && ./miner-panel"
echo