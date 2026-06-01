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
echo "watching per-block intervals (Ctrl+C to stop)..."
printf '%-10s %-15s %-10s %-10s\n' 'block' 'timestamp(s)' 'Δs' 'wall Δms'
printf '%-10s %-15s %-10s %-10s\n' '------' '-------------' '----' '--------'

# POLL_MS controls how often we ask the RPC for the head. 50ms gives ≤50ms
# resolution on per-block wall-clock deltas (block cadence target is 200ms).
POLL_MS="${POLL_MS:-50}"
poll_sleep=$(awk -v ms="$POLL_MS" 'BEGIN { printf "%.3f", ms / 1000 }')

prev_n=$(block_number)
prev_ts=$(block_ts "$prev_n")
prev_wall=$(date +%s%3N)
printf '%-10d %-15d %-10s %-10s\n' "$prev_n" "$prev_ts" '-' '-'

while sleep "$poll_sleep"; do
  cur=$(block_number)
  if (( cur <= prev_n )); then
    continue
  fi
  cur_wall=$(date +%s%3N)
  for ((n=prev_n+1; n<=cur; n++)); do
    ts=$(block_ts "$n")
    delta_s=$(( ts - prev_ts ))
    # If we got more than one new block in a single poll, attribute the wall
    # delta evenly across the missed blocks — only the most recent block
    # delta is truly observed at this poll.
    if (( n == cur )); then
      wall_delta=$(( cur_wall - prev_wall ))
    else
      wall_delta='~'
    fi
    printf '%-10d %-15d %-10s %-10s\n' "$n" "$ts" "$delta_s" "$wall_delta"
    prev_ts=$ts
  done
  prev_n=$cur
  prev_wall=$cur_wall
done
