#!/usr/bin/env bash
# Build HAC OpenCL tools plus CPU-only HACD worker/fullnode on Linux.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

echo
echo "  Building HAC OpenCL tools + CPU-only HACD worker/fullnode..."
echo "  Repo: $REPO_ROOT"
echo

if ! command -v cargo >/dev/null 2>&1; then
  echo "  ERROR: Rust/cargo not found. Install: https://rustup.rs"
  exit 1
fi

# Link-time: libOpenCL.so (runtime: AMDGPU-PRO / ROCm / ocl-icd)
MISSING_DEV=
if ! ldconfig -p 2>/dev/null | grep -q 'libOpenCL\.so'; then
  if [[ ! -f /usr/lib/x86_64-linux-gnu/libOpenCL.so && ! -f /usr/lib/libOpenCL.so ]]; then
    MISSING_DEV=1
  fi
fi
if [[ ! -f /usr/include/CL/cl.h && ! -f /usr/local/include/CL/cl.h ]]; then
  MISSING_DEV=1
fi

if [[ -n "${MISSING_DEV:-}" ]]; then
  echo "  OpenCL development files not found."
  echo "  Debian/Ubuntu:"
  echo "    sudo apt install build-essential ocl-icd-opencl-dev"
  echo "  Fedora:"
  echo "    sudo dnf install ocl-icd-devel"
  echo "  AMD GPU runtime (pick one):"
  echo "    ROCm: https://rocm.docs.amd.com/"
  echo "    or AMDGPU-PRO driver with OpenCL"
  echo
  read -r -p "  Continue build anyway? [y/N] " ans
  [[ "${ans,,}" == "y" ]] || exit 1
fi

cargo build --release --features ocl --bin poworker --bin list_opencl --bin diagnose_opencl
cargo build --release --bin hacash --bin diaworker

echo
echo "  OK: $REPO_ROOT/target/release/poworker"
echo "      $REPO_ROOT/target/release/diaworker"
echo "      $REPO_ROOT/target/release/list_opencl"
echo "      $REPO_ROOT/target/release/diagnose_opencl"
echo "      $REPO_ROOT/target/release/hacash"
echo
echo "  Next:"
echo "    scripts/mining-amd/install-configs.sh"
echo "    scripts/mining-amd/list-opencl-devices.sh"
echo