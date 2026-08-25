#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:-manual}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
RELEASE="$ROOT/target/release"
OUT_DIR="$ROOT/dist-node"
PACKAGE_NAME="hpay-compatible-hacash-fullnode-linux-x86_64"
STAGE="$OUT_DIR/$PACKAGE_NAME"
ARCHIVE="$OUT_DIR/$PACKAGE_NAME-$VERSION.tar.gz"

case "$(uname -m)" in
  x86_64|amd64) ;;
  *) echo "Unsupported release architecture: $(uname -m)"; exit 1 ;;
esac

for required in \
  "$RELEASE/hacash" \
  "$ROOT/mainnet-configs/hacash.config.mainnet.ini" \
  "$ROOT/README-NODE.txt"; do
  [[ -f "$required" ]] || { echo "Missing required node release file: $required"; exit 1; }
done

command -v sha256sum >/dev/null 2>&1 || {
  echo "sha256sum is required to create verifiable release archives"
  exit 1
}

SOURCE_COMMIT="$(git -C "$ROOT" rev-parse HEAD)"
[[ "$SOURCE_COMMIT" =~ ^[0-9a-f]{40}$ ]] || {
  echo "Unable to record an exact 40-character source commit"
  exit 1
}

rm -rf -- "$STAGE"
rm -f -- "$ARCHIVE" "$ARCHIVE.sha256"
mkdir -p "$STAGE"

cp -f "$RELEASE/hacash" "$STAGE/hacash"
cp -f "$ROOT/mainnet-configs/hacash.config.mainnet.ini" \
  "$STAGE/hacash.config.ini.example"
cp -f "$ROOT/README-NODE.txt" "$STAGE/README.txt"
printf '%s' "$VERSION" > "$STAGE/VERSION.txt"
printf '%s' "$SOURCE_COMMIT" > "$STAGE/SOURCE-COMMIT.txt"
chmod u+x "$STAGE/hacash"

for forbidden in \
  poworker diaworker miner-panel hac-pool \
  hbit-pool-server hbit-pool-payout; do
  [[ ! -e "$STAGE/$forbidden" ]] || {
    echo "Standalone node package must not contain $forbidden"
    exit 1
  }
done

tar -czf "$ARCHIVE" -C "$OUT_DIR" "$PACKAGE_NAME"
(cd "$OUT_DIR" && sha256sum "$(basename "$ARCHIVE")" > "$(basename "$ARCHIVE").sha256")
echo "Packaged standalone node: $ARCHIVE"

