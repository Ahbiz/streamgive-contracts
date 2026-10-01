#!/usr/bin/env bash
# Fails if any contract's compiled wasm exceeds its size budget below, and
# always prints each contract's current size so growth is visible in every
# CI run even when a change stays under the limit. Soroban resource fees
# scale partly with contract size, so a creeping regression here is easy to
# miss without an explicit check.
#
# When a change legitimately grows a contract past its budget, raise that
# contract's limit in the same PR rather than working around this script.
#
# Run after building the release wasm32 target:
#
#   cargo build --workspace --target wasm32v1-none --release
#   bash scripts/check-wasm-size.sh

set -euo pipefail

WASM_DIR="target/wasm32v1-none/release"

# Max size in bytes for each contract's compiled wasm.
declare -A MAX_SIZES=(
  [ngo_registry]=65536
  [donation_vault]=98304
)

status=0

for name in "${!MAX_SIZES[@]}"; do
  wasm_path="$WASM_DIR/${name}.wasm"
  max="${MAX_SIZES[$name]}"

  if [[ ! -f "$wasm_path" ]]; then
    echo "error: $wasm_path not found — build the wasm32v1-none release target first" >&2
    status=1
    continue
  fi

  size=$(wc -c < "$wasm_path")
  size_kib=$((size / 1024))
  max_kib=$((max / 1024))

  if (( size > max )); then
    echo "FAIL: ${name}.wasm is ${size} bytes (${size_kib} KiB), exceeding the ${max} byte (${max_kib} KiB) budget"
    status=1
  else
    echo "OK: ${name}.wasm is ${size} bytes (${size_kib} KiB), within the ${max} byte (${max_kib} KiB) budget"
  fi
done

exit "$status"
