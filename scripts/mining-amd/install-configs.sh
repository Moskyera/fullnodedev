#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

FORCE=0
if (( $# > 1 )); then
  echo "Usage: $0 [--force]" >&2
  exit 2
fi
case "${1:-}" in
  "") ;;
  --force) FORCE=1 ;;
  *)
    echo "Usage: $0 [--force]" >&2
    exit 2
    ;;
esac

install_config() {
  local source="$1"
  local destination="$2"

  if [[ -f "$destination" && "$FORCE" -eq 0 ]]; then
    chmod 600 "$destination"
    echo "Kept existing config: $destination"
    return
  fi

  cp -f "$source" "$destination"
  chmod 600 "$destination"
  echo "Installed config: $destination"
}

for profile in debug release; do
  OUT="$REPO_ROOT/target/$profile"
  if [[ -d "$OUT" ]]; then
    install_config "$SCRIPT_DIR/poworker.amd.ini.example" "$OUT/poworker.config.ini"
    install_config "$SCRIPT_DIR/diaworker.amd.ini.example" "$OUT/diaworker.config.ini"
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