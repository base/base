#!/usr/bin/env bash
# Watch block cadence on an L2 RPC endpoint.
#
# Usage:
#   watch-cadence.sh [RPC_URL] [TAIL_N]
#
# Defaults:
#   RPC_URL = http://localhost:7545  (devnet base-builder)
#   TAIL_N  = 12
#
# Output:
#   - Tail of the last N blocks with seconds-timestamp and inter-block delta.
#   - Live measured block rate sampled every second.
#
# Requires: curl, jq. (cast is NOT required.)

set -euo pipefail

RPC_URL="${1:-http://localhost:7545}"
TAIL_N="${2:-12}"

rpc() {
  curl -fsS -X POST -H 'Content-Type: application/json' --data "$1" "$RPC_URL"
}

block_number() {
  rpc '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    | jq -r '.result' | sed 's/^0x//' | { read -r hex; printf '%d' "$((16#$hex))"; }
}

block_ts() {
  local n_hex
  n_hex=$(printf '0x%x' "$1")
  rpc "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getBlockByNumber\",\"params\":[\"$n_hex\",false],\"id\":1}" \
    | jq -r '.result.timestamp' | sed 's/^0x//' | { read -r hex; printf '%d' "$((16#$hex))"; }
}

printf 'RPC: %s\n\n' "$RPC_URL"

latest=$(block_number)
if (( latest == 0 )); then
  echo "no blocks yet (chain head is 0). waiting..." >&2
  while (( latest == 0 )); do
    sleep 1
    latest=$(block_number)
  done
fi

start=$(( latest - TAIL_N + 1 ))
(( start < 0 )) && start=0

printf '%-10s %-15s %-10s\n' 'block' 'timestamp(s)' 'Δs'
printf '%-10s %-15s %-10s\n' '------' '-------------' '----'
prev_ts=
for ((n=start; n<=latest; n++)); do
  ts=$(block_ts "$n")
  if [[ -n "$prev_ts" ]]; then
    delta=$(( ts - prev_ts ))
  else
    delta='-'
  fi
  printf '%-10d %-15d %-10s\n' "$n" "$ts" "$delta"
  prev_ts=$ts
done

echo
echo "sampling live rate (Ctrl+C to stop)..."
prev=$(block_number)
prev_t=$(date +%s%3N)
while sleep 1; do
  cur=$(block_number)
  cur_t=$(date +%s%3N)
  blocks=$(( cur - prev ))
  ms=$(( cur_t - prev_t ))
  if (( blocks > 0 && ms > 0 )); then
    cadence_ms=$(( ms / blocks ))
    printf 'head=%d  +%d blocks / %d ms  →  ~%d ms/block\n' \
      "$cur" "$blocks" "$ms" "$cadence_ms"
  else
    printf 'head=%d  (no new blocks)\n' "$cur"
  fi
  prev=$cur
  prev_t=$cur_t
done
