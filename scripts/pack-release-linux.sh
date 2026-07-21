#!/usr/bin/env bash
# Package prebuilt Linux OpenCL releases from target/release.
set -euo pipefail

VERSION="${1:-dev}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
RELEASE="$ROOT/target/release"
OPENCL="$ROOT/x16rs/opencl"
required_kernels=(
  aes_helper.cl blake.cl bmw.cl cubehash.cl echo.cl fugue.cl groestl.cl
  hamsi.cl hamsi_help.cl hamsi_helper.cl hamsi_helper_big.cl jh.cl
  keccak.cl luffa.cl sha2_512.cl sha3_256.cl shabal.cl shavite.cl
  simd.cl skein.cl util.cl whirlpool.cl x16rs.cl x16rs_diamond.cl x16rs_main.cl
)
OUT_DIR="$ROOT/dist"

case "$(uname -m)" in
  x86_64|amd64) ARCH="x86_64" ;;
  *)
    echo "Unsupported release architecture: $(uname -m) (expected x86_64)"
    exit 1
    ;;
esac

common_binaries=(poworker diaworker list_opencl diagnose_opencl miner-panel)
full_binaries=(hacash "${common_binaries[@]}")

for binary in "${full_binaries[@]}"; do
  if [[ ! -f "$RELEASE/$binary" ]]; then
    echo "Missing binary: $RELEASE/$binary"
    exit 1
  fi
done
for kernel in "${required_kernels[@]}"; do
  if [[ ! -f "$OPENCL/$kernel" ]]; then
    echo "Missing required OpenCL kernel: $OPENCL/$kernel"
    exit 1
  fi
done
if ! command -v sha256sum >/dev/null 2>&1; then
  echo "sha256sum is required to create verifiable release archives."
  exit 1
fi

archive_name() {
  local package_name="$1"
  if [[ "$VERSION" == v* ]]; then
    printf '%s-%s.tar.gz' "$package_name" "$VERSION"
  else
    printf '%s.tar.gz' "$package_name"
  fi
}

copy_common_files() {
  local stage="$1"
  mkdir -p "$stage/x16rs/opencl"
  cp -f "$OPENCL"/*.cl "$stage/x16rs/opencl/"
  cp -f "$ROOT/scripts/mining-amd/poworker.amd.ini.example" \
    "$stage/poworker.config.ini.example"
  cp -f "$ROOT/scripts/mining-amd/diaworker.amd.ini.example" \
    "$stage/diaworker.config.ini.example"
  cp -f "$ROOT/SETUP-LINUX.sh" "$ROOT/START-MINER-PANEL.sh" "$stage/"
  cp -f "$ROOT/docs/MINING-LINUX.md" "$stage/MINING-LINUX.md"
  cp -f "$ROOT/scripts/mining-amd/PRESETS-INDEX.txt" "$stage/PRESETS-INDEX.txt"
  mkdir -p "$stage/presets/poworker" "$stage/presets/diaworker"
  cp -f "$ROOT/scripts/mining-amd/presets/poworker/"*.ini "$stage/presets/poworker/"
  cp -f "$ROOT/scripts/mining-amd/presets/diaworker/"*.ini "$stage/presets/diaworker/"
  if [[ -f "$ROOT/miner-panel/assets/hhh.png" ]]; then
    cp -f "$ROOT/miner-panel/assets/hhh.png" "$stage/hhh.png"
  fi
  printf '%s' "$VERSION" > "$stage/VERSION.txt"
  chmod u+x "$stage/SETUP-LINUX.sh" "$stage/START-MINER-PANEL.sh"
  chmod u+x "$stage/poworker" "$stage/diaworker" "$stage/list_opencl" \
    "$stage/diagnose_opencl" "$stage/miner-panel"
}

write_fullnode_example() {
  local path="$1"
  cat > "$path" <<'EOF'
[node]
fast_sync = false

[server]
enable = true
listen = 8080
diamond_form = true
debug_open = false

[miner]
enable = false
reward = YOUR_HAC_WALLET_ADDRESS
message = hac_miner_linux

[diamondminer]
enable = false
reward = YOUR_HACD_PRIVAKEY_3x
EOF
}

pack_flavor() {
  local flavor="$1"
  local package_name="hacash-miner-${flavor}-linux-${ARCH}"
  local stage="$OUT_DIR/$package_name"
  local archive="$OUT_DIR/$(archive_name "$package_name")"
  shift
  local -a binaries=("$@")

  # Both paths are exact children of this repository's dist directory.
  rm -rf -- "$stage"
  rm -f -- "$archive" "$archive.sha256"
  mkdir -p "$stage"

  for binary in "${binaries[@]}"; do
    cp -f "$RELEASE/$binary" "$stage/$binary"
  done
  copy_common_files "$stage"

  if [[ "$flavor" == "full" ]]; then
    write_fullnode_example "$stage/hacash.config.ini.example"
    cp -f "$ROOT/README-LINUX-RELEASE.txt" "$stage/README-LINUX.txt"
    chmod u+x "$stage/hacash"
  else
    cp -f "$ROOT/README-LINUX-MINER-ONLY.txt" "$stage/README-LINUX.txt"
  fi

  tar -czf "$archive" -C "$OUT_DIR" "$package_name"
  (cd "$OUT_DIR" && sha256sum "$(basename "$archive")" > "$(basename "$archive").sha256")
  echo "Packaged: $archive"
}

mkdir -p "$OUT_DIR"
pack_flavor only "${common_binaries[@]}"
pack_flavor full "${full_binaries[@]}"
