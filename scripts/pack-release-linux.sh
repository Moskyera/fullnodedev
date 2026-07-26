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
# HBIT pool operator files. The pool holds real miner money, so the runbook and
# the argument worksheet ship WITH the binaries: a runbook on a web page the
# operator never opens is the same as no runbook.
POOL_RUNBOOK="$ROOT/docs/POOL-OPERATOR.md"
POOL_WORKSHEET="$ROOT/hbit-pool/hbit-pool.example.ini"
POOL_README="$ROOT/README-POOL.txt"

case "$(uname -m)" in
  x86_64|amd64) ARCH="x86_64" ;;
  *)
    echo "Unsupported release architecture: $(uname -m) (expected x86_64)"
    exit 1
    ;;
esac

common_binaries=(poworker diaworker list_opencl diagnose_opencl miner-panel)
full_binaries=(hacash "${common_binaries[@]}")
# HBIT payout pool. FULL package only: it needs a synced fullnode of its own to
# fetch templates, submit blocks and settle, and the full package is the one
# that ships that node. The miner-only package is the small worker-rig payload
# for someone who just wants to mine, and a wallet-holding daemon they will
# never start is clutter with a downside.
pool_binaries=(hbit-pool-server hbit-pool-payout)

for binary in "${full_binaries[@]}"; do
  if [[ ! -f "$RELEASE/$binary" ]]; then
    echo "Missing binary: $RELEASE/$binary"
    exit 1
  fi
done
for binary in "${pool_binaries[@]}"; do
  if [[ ! -f "$RELEASE/$binary" ]]; then
    echo "Missing binary: $RELEASE/$binary"
    echo "Build it with: cargo build --release -p hbit-pool"
    exit 1
  fi
done
for doc in "$POOL_RUNBOOK" "$POOL_WORKSHEET" "$POOL_README"; do
  if [[ ! -f "$doc" ]]; then
    echo "Missing required HBIT pool file: $doc"
    if [[ "$doc" == "$POOL_WORKSHEET" ]]; then
      # .gitignore blanket-ignores *.ini and needs one explicit negation per
      # shipped template, the way it already carries one for the sibling pool.
      # Without that line the worksheet exists on the author's disk but never
      # reaches a clone or a CI checkout, and this is the only place that says so.
      echo "A checkout missing only this file usually means .gitignore lacks the line"
      echo "  !hbit-pool/hbit-pool.example.ini"
      echo "which belongs next to the existing !miner-pool/hac-pool.example.ini"
    fi
    exit 1
  fi
done
# The worksheet documents the ARGUMENTS hbit-pool-server takes; it must ship
# with every operator-specific answer BLANK. A shipped node URL, wallet path,
# listen address or chain is a value somebody pastes without reading, and one of
# them decides which wallet holds other people's mining income.
if filled=$(grep -nE '^[[:space:]]*(node|wallet_file|listen|chain|pool_base)[[:space:]]*=[[:space:]]*[^[:space:]]' \
  "$POOL_WORKSHEET"); then
  echo "hbit-pool.example.ini ships a filled-in operator value:"
  echo "$filled"
  exit 1
fi
# Nothing an operator receives may carry a live address, private key or
# passphrase. An earlier audit found a shipped template with a valid third-party
# reward address in it, which would have paid a stranger; these two files are
# now scanned for the same class of mistake, comments included.
for doc in "$POOL_WORKSHEET" "$POOL_README"; do
  if hit=$(grep -oE '\b1[1-9A-HJ-NP-Za-km-z]{25,34}\b' "$doc"); then
    echo "$(basename "$doc") ships something shaped like a live HAC address: $hit"
    exit 1
  fi
  if hit=$(grep -oE '\b[0-9a-fA-F]{64}\b' "$doc"); then
    echo "$(basename "$doc") ships something shaped like a private key: $hit"
    exit 1
  fi
  if hit=$(grep -niE '^[[:space:]]*(password|passphrase|HBIT_WALLET_PASSWORD[A-Za-z_]*)[[:space:]]*=[[:space:]]*[^[:space:]]' "$doc"); then
    echo "$(basename "$doc") must never carry a passphrase value: $hit"
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

copy_pool_assets() {
  local stage="$1"

  for binary in "${pool_binaries[@]}"; do
    cp -f "$RELEASE/$binary" "$stage/$binary"
    chmod u+x "$stage/$binary"
  done
  # The runbook and the worksheet travel with the binaries. Shipping the pool
  # without them is what the earlier refusal to package it was about.
  cp -f "$POOL_RUNBOOK" "$stage/POOL-OPERATOR.md"
  cp -f "$POOL_WORKSHEET" "$stage/hbit-pool.example.ini"
  cp -f "$POOL_README" "$stage/README-POOL.txt"

  for name in hbit-pool-server hbit-pool-payout POOL-OPERATOR.md \
    hbit-pool.example.ini README-POOL.txt; do
    if [[ ! -f "$stage/$name" ]]; then
      echo "Staged package is missing $name"
      exit 1
    fi
  done
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
    copy_pool_assets "$stage"
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
