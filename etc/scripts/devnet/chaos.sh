#!/usr/bin/env bash
# Injects faults into the running local Docker devnet and reports how the chain
# reacts. Scenarios, expected behavior, and signals: docs/guides/DEVNET_CHAOS.md.
#
# Usage:
#   chaos.sh batcher-outage [secs]   # stop op-batcher for secs (default 60), then restart it
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
cd "$SCRIPT_DIR/../../.."

COMPOSE=(docker compose --env-file etc/docker/devnet-env -f etc/docker/docker-compose.yml)
POLL_SECS=2
# Time the safe head gets after the batcher restarts to catch up with the unsafe
# head recorded at the end of the outage, on top of the outage length itself.
RECOVERY_GRACE_SECS=300

usage() {
  echo "Usage: just devnet chaos <scenario> [args]"
  echo ""
  echo "Implemented scenarios:"
  echo "  batcher-outage [secs]   stop the canonical op-batcher for secs (default 60), then restart it"
  echo ""
  echo "Planned scenarios and what each one tests: docs/guides/DEVNET_CHAOS.md"
}

die() {
  echo "ERROR: $*" >&2
  exit 1
}

# Refuses to run unless every named service of the local devnet compose project is running.
require_running() {
  local running
  running=$("${COMPOSE[@]}" ps --status running --format '{{.Service}}' 2>/dev/null) \
    || die "cannot list devnet containers; is Docker running?"
  [ -n "$running" ] || die "the local devnet is not running; start it with 'just devnet up' or 'just devnet up-single'"
  for service in "$@"; do
    grep -qx "$service" <<<"$running" \
      || die "devnet service '$service' is not running; restart it with 'docker start $service' or restart the devnet"
  done
}

# Prints "<unsafe> <safe>" L2 block numbers from a node's optimism_syncStatus, or "? ?".
sync_heads() {
  curl -fsS --max-time 2 -X POST -H "Content-Type: application/json" \
    --data '{"jsonrpc":"2.0","method":"optimism_syncStatus","params":[],"id":1}' "$1" 2>/dev/null \
    | jq -r '"\(.result.unsafe_l2.number // "?") \(.result.safe_l2.number // "?")"' 2>/dev/null \
    || echo "? ?"
}

# Prints one status row and sets UNSAFE and SAFE to the builder's heads.
print_row() {
  local phase=$1 elapsed=$2 l1 client_safe lag
  l1=$(cast block-number --rpc-url "$L1_RPC_URL" 2>/dev/null || echo "?")
  read -r UNSAFE SAFE <<<"$(sync_heads "$L2_BUILDER_OP_RPC_URL")"
  read -r _ client_safe <<<"$(sync_heads "$L2_CLIENT_OP_RPC_URL")"
  lag="?"
  [[ "$UNSAFE" =~ ^[0-9]+$ && "$SAFE" =~ ^[0-9]+$ ]] && lag=$((UNSAFE - SAFE))
  printf "%-9s %6ss  %6s  %14s  %12s  %8s  %11s\n" \
    "$phase" "$elapsed" "$l1" "$UNSAFE" "$SAFE" "$lag" "$client_safe"
}

print_header() {
  printf "%-9s %7s  %6s  %14s  %12s  %8s  %11s\n" \
    "phase" "elapsed" "L1" "builder unsafe" "builder safe" "safe lag" "client safe"
}

# Plain `docker start` on the container ID: `docker compose start` would also rerun
# op-batcher's dependencies, including the one-shot setup-devnet container.
BATCHER_CONTAINER=""
restart_batcher() {
  if [ -n "$BATCHER_CONTAINER" ]; then
    docker start "$BATCHER_CONTAINER" >/dev/null
    BATCHER_CONTAINER=""
  fi
}

batcher_outage() {
  local secs=${1:-60}
  [[ "$secs" =~ ^[1-9][0-9]*$ ]] || die "outage length must be a positive number of seconds (got '$secs')"

  require_running l1-el base-builder base-client op-batcher

  read -r UNSAFE SAFE <<<"$(sync_heads "$L2_BUILDER_OP_RPC_URL")"
  [[ "$UNSAFE" =~ ^[0-9]+$ && "$SAFE" =~ ^[0-9]+$ ]] \
    || die "base-builder optimism_syncStatus is not answering on $L2_BUILDER_OP_RPC_URL"
  local start_unsafe=$UNSAFE start_safe=$SAFE

  echo "Batcher outage: stopping op-batcher for ${secs}s."
  echo "Expect the safe head to stall while the unsafe head keeps advancing, then catch up."
  echo "The client derives ${BASE_NODE_VERIFIER_L1_CONFS:-0} L1 blocks behind the L1 head, so its safe head reacts later."
  echo ""

  # Restart op-batcher however the script exits, including Ctrl+C.
  trap restart_batcher EXIT
  trap 'exit 130' INT TERM

  # op-batcher stops with SIGKILL, so this is a crash, not a graceful drain. A batch
  # transaction already in the L1 mempool can still land and move the safe head once.
  BATCHER_CONTAINER=$("${COMPOSE[@]}" ps -q op-batcher)
  docker stop "$BATCHER_CONTAINER" >/dev/null
  local started=$SECONDS
  print_header
  while [ $((SECONDS - started)) -lt "$secs" ]; do
    print_row outage $((SECONDS - started))
    sleep "$POLL_SECS"
  done
  print_row outage $((SECONDS - started))
  local outage_unsafe=$UNSAFE outage_safe=$SAFE

  restart_batcher
  local restarted_at=$SECONDS
  echo "op-batcher restarted; waiting for the builder safe head to reach $outage_unsafe."
  local deadline=$((SECONDS + secs + RECOVERY_GRACE_SECS)) recovered=0
  while [ "$SECONDS" -lt "$deadline" ]; do
    print_row recovery $((SECONDS - started))
    if [[ "$SAFE" =~ ^[0-9]+$ ]] && [ "$SAFE" -ge "$outage_unsafe" ]; then
      recovered=1
      break
    fi
    sleep "$POLL_SECS"
  done

  echo ""
  echo "Unsafe head during outage: $start_unsafe -> $outage_unsafe (+$((outage_unsafe - start_unsafe)))"
  echo "Safe head during outage:   $start_safe -> $outage_safe (+$((outage_safe - start_safe)))"
  echo "Safe lag:                  $((start_unsafe - start_safe)) -> $((outage_unsafe - outage_safe)) blocks"

  [ "$outage_unsafe" -gt "$start_unsafe" ] \
    || die "the unsafe head did not advance during the outage; the sequencer is not producing blocks"
  [ $((outage_unsafe - outage_safe)) -gt $((start_unsafe - start_safe)) ] \
    || echo "WARNING: the safe lag did not grow; the outage may be shorter than the normal batch interval"
  [ "$recovered" = 1 ] \
    || die "the safe head did not reach $outage_unsafe within $((secs + RECOVERY_GRACE_SECS))s of the restart"
  echo "Recovered: the safe head reached $outage_unsafe $((SECONDS - restarted_at))s after op-batcher restarted."
}

scenario=${1:-}
[ $# -gt 0 ] && shift
case "$scenario" in
  batcher-outage) batcher_outage "$@" ;;
  "" | -h | --help | help) usage ;;
  *)
    usage >&2
    echo "" >&2
    die "unknown or not yet implemented scenario: $scenario"
    ;;
esac
