#!/usr/bin/env bash
# Package Linux OpenCL miner release tarball from target/release.
set -euo pipefail

VERSION="${1:-dev}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
RELEASE="$ROOT/target/release"
OPENCL="$ROOT/x16rs/opencl"
OUT_DIR="$ROOT/dist"
PKG="hacash-miner-linux-x64-${VERSION}"
STAGE="$OUT_DIR/$PKG"

BINARIES=(hacash poworker diaworker list_opencl)
OPTIONAL_BINARIES=(miner-panel)

if [[ ! -d "$RELEASE" ]]; then
  echo "Missing $RELEASE — run scripts/mining-amd/build-amd-miner.sh first."
  exit 1
fi

for b in "${BINARIES[@]}"; do
  if [[ ! -x "$RELEASE/$b" ]]; then
    echo "Missing binary: $RELEASE/$b"
    exit 1
  fi
done

if [[ ! -f "$OPENCL/x16rs_main.cl" ]]; then
  echo "Missing OpenCL kernels: $OPENCL"
  exit 1
fi

rm -rf "$STAGE"
mkdir -p "$STAGE/x16rs/opencl"

for b in "${BINARIES[@]}"; do
  cp -f "$RELEASE/$b" "$STAGE/"
done

for b in "${OPTIONAL_BINARIES[@]}"; do
  if [[ -x "$RELEASE/$b" ]]; then
    cp -f "$RELEASE/$b" "$STAGE/"
  fi
done

cp -f "$OPENCL"/*.cl "$STAGE/x16rs/opencl/"

if [[ -f "$ROOT/scripts/mining-amd/poworker.amd.ini.example" ]]; then
  cp -f "$ROOT/scripts/mining-amd/poworker.amd.ini.example" "$STAGE/poworker.config.ini.example"
  cp -f "$ROOT/scripts/mining-amd/diaworker.amd.ini.example" "$STAGE/diaworker.config.ini.example"
fi

if [[ -f "$ROOT/hacash.config.ini" ]]; then
  cp -f "$ROOT/hacash.config.ini" "$STAGE/hacash.config.ini.example"
fi

cat > "$STAGE/README-LINUX.txt" <<'EOF'
Hacash OpenCL miner — Linux x64

1. Install ROCm or AMDGPU-PRO OpenCL runtime
2. cp poworker.config.ini.example poworker.config.ini
3. ./list_opencl  → set platform_id / device_ids in ini
4. cp hacash.config.ini.example hacash.config.ini  (edit reward address)
5. ./hacash &
6. sleep 35 && ./poworker

Optional GUI: ./miner-panel (needs libxcb-render0 libgtk-3-0)

Docs: docs/MINING-LINUX.md
EOF

mkdir -p "$OUT_DIR"
tar -czf "$OUT_DIR/${PKG}.tar.gz" -C "$OUT_DIR" "$PKG"
echo "OK: $OUT_DIR/${PKG}.tar.gz"