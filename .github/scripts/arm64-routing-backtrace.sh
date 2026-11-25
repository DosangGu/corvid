#!/usr/bin/env bash
set -euo pipefail

core_dir=$(mktemp -d "$RUNNER_TEMP/routing-cores.XXXXXX")
core_pattern=/proc/sys/kernel/core_pattern
original_pattern=$(<"$core_pattern")
restore_pattern=false

cleanup() {
  if $restore_pattern; then
    printf '%s\n' "$original_pattern" | sudo tee "$core_pattern" >/dev/null || true
  fi
  rm -f -- "$core_dir"/core.*
  rmdir -- "$core_dir" || true
}
trap cleanup EXIT

if printf '%s\n' "$core_dir/core.%E.%p" | sudo tee "$core_pattern" >/dev/null; then
  restore_pattern=true
  ulimit -c 262144 || echo '::warning::Could not enable routing core dumps'
else
  echo '::warning::Could not configure routing core dumps'
fi

status=0
"$@" || status=$?
if (( status == 0 )); then
  exit 0
fi

for core in "$core_dir"/core.*; do
  [[ -f "$core" ]] || continue
  encoded_executable=${core##*/core.}
  encoded_executable=${encoded_executable%.*}
  executable=${encoded_executable//!/\/}
  case "$executable" in
    "$GITHUB_WORKSPACE"/target/aarch64-unknown-linux-musl/release/*/routing-*)
      echo "Routing backtrace for ${executable##*/}"
      timeout 30s gdb --batch -nx -iex 'set debuginfod enabled off' -iex 'set auto-load off' \
        -ex 'set print frame-arguments none' -ex 'thread apply all bt 20' "$executable" "$core" || true
      ;;
  esac
done
exit "$status"
