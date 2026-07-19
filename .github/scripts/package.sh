#!/usr/bin/env bash
# Package a built vmcp binary into dist/ as a per-platform archive.
#
#   package.sh <archive-base-name> <path-to-binary>
#
# Produces dist/<archive-base-name>.zip on Windows runners and
# dist/<archive-base-name>.tar.gz everywhere else. The binary is placed at the
# archive root under its original file name (vmcp or vmcp.exe).
set -euo pipefail

base="$1"
bin_path="$2"

if [[ ! -f "$bin_path" ]]; then
  echo "package.sh: binary not found: $bin_path" >&2
  exit 1
fi

mkdir -p dist
bin_dir="$(dirname "$bin_path")"
bin_name="$(basename "$bin_path")"

if [[ "${RUNNER_OS:-}" == "Windows" ]]; then
  # 7z ships on GitHub Windows runners. Add the binary at the archive root.
  ( cd "$bin_dir" && 7z a -tzip "$(pwd)/tmp.zip" "$bin_name" >/dev/null )
  mv "$bin_dir/tmp.zip" "dist/${base}.zip"
  echo "packaged dist/${base}.zip"
else
  tar -C "$bin_dir" -czf "dist/${base}.tar.gz" "$bin_name"
  echo "packaged dist/${base}.tar.gz"
fi
