#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

expected_preset_count=23
pow_dir="scripts/mining-amd/presets/poworker"
dia_dir="scripts/mining-amd/presets/diaworker"
index_file="scripts/mining-amd/PRESETS-INDEX.txt"

shopt -s nullglob
pow_files=("$pow_dir"/*.ini)
dia_files=("$dia_dir"/*.ini)

if (( ${#pow_files[@]} != expected_preset_count || ${#dia_files[@]} != expected_preset_count )); then
  echo "Expected $expected_preset_count poworker and diaworker presets; found ${#pow_files[@]} and ${#dia_files[@]}" >&2
  exit 1
fi

preset_names() {
  local file
  for file in "$@"; do
    printf '%s\n' "${file##*/}"
  done | sort
}

mapfile -t pow_names < <(preset_names "${pow_files[@]}")
mapfile -t dia_names < <(preset_names "${dia_files[@]}")
mapfile -t indexed_names < <(
  sed -nE 's/^\[[0-9]+\][[:space:]]+([^[:space:]]+).*/\1.ini/p' "$index_file" | sort
)

if [[ "${pow_names[*]}" != "${dia_names[*]}" ]]; then
  echo "Poworker and diaworker preset names do not match" >&2
  diff -u <(printf '%s\n' "${pow_names[@]}") <(printf '%s\n' "${dia_names[@]}") || true
  exit 1
fi
if [[ "${pow_names[*]}" != "${indexed_names[*]}" ]]; then
  echo "PRESETS-INDEX.txt does not list the complete preset set" >&2
  diff -u <(printf '%s\n' "${pow_names[@]}") <(printf '%s\n' "${indexed_names[@]}") || true
  exit 1
fi

for preset in "${dia_files[@]}"; do
  grep -Eq '^use_opencl[[:space:]]*=[[:space:]]*false[[:space:]]*$' "$preset"
  grep -Eq '^use_cuda[[:space:]]*=[[:space:]]*false[[:space:]]*$' "$preset"
done

for preset in "${pow_files[@]}"; do
  if grep -Eq '^use_cuda[[:space:]]*=[[:space:]]*true[[:space:]]*$' "$preset"; then
    echo "CUDA must stay disabled in OpenCL presets: $preset" >&2
    exit 1
  fi
  case "${preset##*/}" in
    cpu-only-*)
      grep -Eq '^use_opencl[[:space:]]*=[[:space:]]*false[[:space:]]*$' "$preset"
      ;;
    *)
      grep -Eq '^use_opencl[[:space:]]*=[[:space:]]*true[[:space:]]*$' "$preset"
      ;;
  esac
done

rx9070_presets=("$pow_dir"/*rx9070xt*.ini)
if (("${#rx9070_presets[@]}" == 0)); then
  echo "No RX 9070 XT poworker presets found" >&2
  exit 1
fi
for preset in "${rx9070_presets[@]}"; do
  grep -Eq '^gpu_profile[[:space:]]*=[[:space:]]*amd_balanced[[:space:]]*$' "$preset"
  grep -Eq '^work_groups[[:space:]]*=[[:space:]]*64[[:space:]]*$' "$preset"
  grep -Eq '^unit_size[[:space:]]*=[[:space:]]*64[[:space:]]*$' "$preset"
done

if grep -E 'CL_OUT_OF_RESOURCES|~3 MH/s|work_groups=1024' miner-panel/src/i18n.rs; then
  echo "Stale RDNA4 failure guidance found in panel translations" >&2
  exit 1
fi

grep -Eq 'Slug = "rx9070xt".*Profile = "amd_balanced".*WorkGroups = 64.*UnitSize = 64' \
  scripts/mining-amd/GENERATE-MINING-CONFIG.ps1

echo "Mining preset safety and completeness checks passed."