#!/usr/bin/env bash
# First-time setup for the prebuilt Linux x86_64 release package.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

NO_LAUNCH=0
if [[ "${1:-}" == "--no-launch" || "${HACASH_SETUP_NO_LAUNCH:-0}" == "1" ]]; then
  NO_LAUNCH=1
fi

echo
echo "============================================"
echo " HAC Miner setup (Linux x86_64)"
echo " By Mosky"
echo "============================================"
echo

required=(poworker diaworker list_opencl diagnose_opencl miner-panel)
if [[ -f "$SCRIPT_DIR/hacash" ]]; then
  required+=(hacash)
  package_kind="full"
else
  package_kind="miner-only"
fi

missing=0
for binary in "${required[@]}"; do
  if [[ ! -f "$SCRIPT_DIR/$binary" ]]; then
    echo "[MISSING] $binary"
    missing=1
  else
    chmod u+x "$SCRIPT_DIR/$binary"
  fi
done
if (( missing )); then
  echo
  echo "This package is incomplete. Download the complete Linux x86_64 release."
  exit 1
fi

if [[ ! -f "$SCRIPT_DIR/x16rs/opencl/x16rs_main.cl" ]]; then
  echo "[MISSING] x16rs/opencl/x16rs_main.cl"
  exit 1
fi
echo "[OK] Binaries and OpenCL kernels found."

if [[ ! -f poworker.config.ini ]]; then
  cp poworker.config.ini.example poworker.config.ini
  echo "[CREATED] poworker.config.ini"
fi
if [[ ! -f diaworker.config.ini ]]; then
  cp diaworker.config.ini.example diaworker.config.ini
  echo "[CREATED] diaworker.config.ini (HACD CPU-only)"
fi

# Release archives use a flat folder, so the kernels are relative to this folder.
# Never overwrite a non-empty custom pool/RPC connect on re-run.
sed -E -i 's|^opencl_dir[[:space:]]*=.*$|opencl_dir = x16rs/opencl/|' poworker.config.ini
for f in poworker.config.ini diaworker.config.ini; do
  [[ -f "$f" ]] || continue
  if grep -Eq '^connect[[:space:]]*=[[:space:]]*[^[:space:]]' "$f"; then
    : # keep custom connect
  elif grep -Eq '^connect[[:space:]]*=' "$f"; then
    sed -E -i 's|^connect[[:space:]]*=.*$|connect = 127.0.0.1:8080|' "$f"
  else
    printf 'connect = 127.0.0.1:8080\n%s\n' "$(cat "$f")" >"$f.tmp" && mv "$f.tmp" "$f"
  fi
done

if [[ "$package_kind" == "full" && ! -f hacash.config.ini ]]; then
  cp hacash.config.ini.example hacash.config.ini
  echo "[CREATED] hacash.config.ini"
fi

# Mining/fullnode configs may contain private keys or remote RPC credentials.
config_files=(poworker.config.ini diaworker.config.ini)
if [[ -f hacash.config.ini ]]; then
  config_files+=(hacash.config.ini)
fi
chmod 600 "${config_files[@]}"

ubuntu_runtime_packages=(
  ocl-icd-libopencl1 clinfo
  libx11-6 libx11-xcb1 libxcursor1 libxi6
  libxkbcommon0 libxkbcommon-x11-0
  libwayland-client0 libegl1 libgl1
  libxcb1 libxcb-render0 libxcb-shape0 libxcb-xfixes0
)

has_opencl_loader() {
  ldconfig -p 2>/dev/null | grep -q 'libOpenCL.so.1'     || [[ -f /usr/lib/x86_64-linux-gnu/libOpenCL.so.1 ]]     || [[ -f /usr/lib64/libOpenCL.so.1 ]]
}

missing_ubuntu_runtime=()
find_missing_ubuntu_runtime() {
  missing_ubuntu_runtime=()
  command -v dpkg-query >/dev/null 2>&1 || return 1
  local package status
  for package in "${ubuntu_runtime_packages[@]}"; do
    status="$(dpkg-query -W -f='${Status}' "$package" 2>/dev/null || true)"
    if [[ "$status" != "install ok installed" ]]; then
      missing_ubuntu_runtime+=("$package")
    fi
  done
  ((${#missing_ubuntu_runtime[@]} > 0))
}

install_ubuntu_runtime() {
  local -a elevate=()
  if (( EUID != 0 )); then
    if command -v sudo >/dev/null 2>&1; then
      elevate=(sudo)
    else
      echo "sudo is not installed. Install the runtime packages as administrator."
      return 1
    fi
  fi
  "${elevate[@]}" apt-get update
  "${elevate[@]}" apt-get install -y "${ubuntu_runtime_packages[@]}"
}

runtime_help_needed=0
if ! has_opencl_loader; then
  echo
  echo "The standard OpenCL loader is missing."
  runtime_help_needed=1
fi
if command -v apt-get >/dev/null 2>&1 && find_missing_ubuntu_runtime; then
  echo
  echo "Missing Linux runtime packages: ${missing_ubuntu_runtime[*]}"
  runtime_help_needed=1
fi

if (( runtime_help_needed )); then
  if command -v apt-get >/dev/null 2>&1; then
    answer=n
    if [[ -t 0 ]]; then
      read -r -p "Install the safe Ubuntu/Debian runtime libraries now? [Y/n] " answer || answer=n
      answer="${answer:-y}"
    else
      echo "Non-interactive setup: package installation skipped."
    fi
    if [[ ! "$answer" =~ ^[Nn]$ ]]; then
      install_ubuntu_runtime || true
    fi
  else
    echo "Install your distribution's OpenCL ICD loader and desktop runtime libraries."
  fi
fi
echo
echo "Checking OpenCL devices (used by HAC only)..."
if ./list_opencl; then
  echo "[OK] OpenCL diagnostic completed."
else
  echo
  echo "[WARNING] No usable OpenCL GPU was found."
  echo "Install the GPU vendor's Linux OpenCL driver, then run SETUP-LINUX.sh again."
  echo "AMD/RX 9000: https://rocm.docs.amd.com/projects/install-on-linux/en/latest/"
  echo "HACD remains available because it is CPU-only."
fi

# Create a convenient desktop launcher beside the application.
launcher="$SCRIPT_DIR/HAC-Miner-Panel.desktop"
{
  echo '[Desktop Entry]'
  echo 'Type=Application'
  echo 'Name=HAC Miner Panel'
  echo 'Comment=Hacash OpenCL miner dashboard'
  printf 'Exec=bash "%s/START-MINER-PANEL.sh"\n' "$SCRIPT_DIR"
  printf 'Path=%s\n' "$SCRIPT_DIR"
  if [[ -f "$SCRIPT_DIR/hhh.png" ]]; then
    printf 'Icon=%s/hhh.png\n' "$SCRIPT_DIR"
  fi
  echo 'Terminal=true'
  echo 'Categories=Utility;'
} > "$launcher"
chmod u+x "$launcher" START-MINER-PANEL.sh SETUP-LINUX.sh

echo
echo "============================================"
echo " Setup complete"
echo "============================================"
echo "HAC  : OpenCL GPU mining + automatic detection/Auto Tune"
echo "HACD : CPU/fullnode mining; OpenCL is not used"
echo "Fleet: enable LAN sharing in Settings on remote miners"
echo

if (( NO_LAUNCH == 0 )); then
  if [[ -n "${DISPLAY:-}" || -n "${WAYLAND_DISPLAY:-}" ]]; then
    read -r -p "Open HAC Miner Panel now? [Y/n] " answer
    if [[ ! "${answer:-y}" =~ ^[Nn]$ ]]; then
      exec "$SCRIPT_DIR/miner-panel"
    fi
  else
    echo "No Linux desktop session was detected. Later run: ./START-MINER-PANEL.sh"
  fi
fi

