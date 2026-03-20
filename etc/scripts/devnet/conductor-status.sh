#!/bin/bash
# Real-time dashboard for HA sequencer status: conductor role, unsafe L2 block,
# and P2P peer count for all three sequencer/conductor pairs. Refreshes in-place
# without flickering using ANSI cursor positioning.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEVNET_ENV="$SCRIPT_DIR/../../docker/devnet-env"

if [ -f "$DEVNET_ENV" ]; then
  # shellcheck source=/dev/null
  source "$DEVNET_ENV"
fi

CONDUCTOR0_RPC_PORT="${CONDUCTOR0_RPC_PORT:-6545}"
CONDUCTOR1_RPC_PORT="${CONDUCTOR1_RPC_PORT:-6546}"
CONDUCTOR2_RPC_PORT="${CONDUCTOR2_RPC_PORT:-6547}"
L2_BUILDER_CL_RPC_PORT="${L2_BUILDER_CL_RPC_PORT:-7549}"
L2_SEQ1_CL_RPC_PORT="${L2_SEQ1_CL_RPC_PORT:-10549}"
L2_SEQ2_CL_RPC_PORT="${L2_SEQ2_CL_RPC_PORT:-11549}"

CONDUCTOR_PORTS=("$CONDUCTOR0_RPC_PORT" "$CONDUCTOR1_RPC_PORT" "$CONDUCTOR2_RPC_PORT")
CONDUCTOR_NAMES=("op-conductor-0  " "op-conductor-1  " "op-conductor-2  ")
CL_NAMES=("base-builder-cl     " "base-sequencer-1-cl " "base-sequencer-2-cl ")
CL_PORTS=("$L2_BUILDER_CL_RPC_PORT" "$L2_SEQ1_CL_RPC_PORT" "$L2_SEQ2_CL_RPC_PORT")

REFRESH="${SEQUENCER_STATUS_INTERVAL:-0.2}"

# ANSI
BOLD='\033[1m'
DIM='\033[2m'
YELLOW='\033[1;33m'
GREEN='\033[0;32m'
RED='\033[0;31m'
CYAN='\033[0;36m'
RESET='\033[0m'

TMPDIR_DATA=$(mktemp -d)

cleanup() {
  tput cnorm 2>/dev/null || true
  rm -rf "$TMPDIR_DATA"
  echo
  exit 0
}
trap cleanup INT TERM

# Fetch data for one node; writes "leader|unsafe_block|peers" to a temp file.
fetch_node() {
  local c_port=$1
  local cl_port=$2
  local out=$3

  local leader unsafe peers
  leader="down"
  unsafe="?"
  peers="?"

  local resp
  if resp=$(curl -sf --max-time 1 \
      -X POST -H "Content-Type: application/json" \
      --data '{"jsonrpc":"2.0","method":"conductor_leader","params":[],"id":1}' \
      "http://localhost:$c_port" 2>/dev/null); then
    leader=$(printf '%s' "$resp" | jq -r 'if .result == null then "?" else (.result | tostring) end' 2>/dev/null || echo "?")
  fi

  if resp=$(curl -sf --max-time 1 \
      -X POST -H "Content-Type: application/json" \
      --data '{"jsonrpc":"2.0","method":"optimism_syncStatus","params":[],"id":1}' \
      "http://localhost:$cl_port" 2>/dev/null); then
    unsafe=$(printf '%s' "$resp" | jq -r '.result.unsafe_l2.number // "?"' 2>/dev/null || echo "?")
  fi

  if resp=$(curl -sf --max-time 1 \
      -X POST -H "Content-Type: application/json" \
      --data '{"jsonrpc":"2.0","method":"opp2p_peerStats","params":[],"id":1}' \
      "http://localhost:$cl_port" 2>/dev/null); then
    peers=$(printf '%s' "$resp" | jq -r '.result.connected // "?"' 2>/dev/null || echo "?")
  fi

  printf '%s|%s|%s\n' "$leader" "$unsafe" "$peers" > "$out"
}

# Render one column cell, padding to a fixed width.
cell() {
  local content=$1
  local width=${2:-20}
  printf "%-*s" "$width" "$content"
}

W=22  # column width

draw() {
  local -a leaders=() unsafes=() peerss=()
  for i in 0 1 2; do
    local raw
    raw=$(cat "${TMPDIR_DATA}/$i" 2>/dev/null || printf 'down|?|?')
    IFS='|' read -r leaders[$i] unsafes[$i] peerss[$i] <<< "$raw"
  done

  # Move to top-left without clearing (no flicker)
  printf '\033[H'

  # ── Title ──────────────────────────────────────────────────────────────────
  printf "${BOLD}  HA Sequencer Status${RESET}  "
  printf "$(date '+%H:%M:%S')"
  printf "   (Ctrl-C to quit)\n\n"

  # ── Conductor names ────────────────────────────────────────────────────────
  printf "  %16s" ""
  for i in 0 1 2; do
    if [ "${leaders[$i]}" = "true" ]; then
      printf "${BOLD}${YELLOW}$(cell "${CONDUCTOR_NAMES[$i]}" $W)${RESET}"
    else
      printf "${DIM}$(cell "${CONDUCTOR_NAMES[$i]}" $W)${RESET}"
    fi
  done
  printf '\n'

  # ── CL names ───────────────────────────────────────────────────────────────
  printf "  %16s" ""
  for i in 0 1 2; do
    if [ "${leaders[$i]}" = "true" ]; then
      printf "${CYAN}$(cell "${CL_NAMES[$i]}" $W)${RESET}"
    else
      printf "${DIM}$(cell "${CL_NAMES[$i]}" $W)${RESET}"
    fi
  done
  printf '\n\n'

  # ── Role ───────────────────────────────────────────────────────────────────
  printf "  ${BOLD}%-16s${RESET}" "Role"
  for i in 0 1 2; do
    case "${leaders[$i]}" in
      true)
        printf "${BOLD}${YELLOW}$(cell "★  LEADER" $W)${RESET}"
        ;;
      false)
        printf "${DIM}$(cell "   follower" $W)${RESET}"
        ;;
      down)
        printf "${RED}$(cell "   offline" $W)${RESET}"
        ;;
      *)
        printf "$(cell "   unknown" $W)"
        ;;
    esac
  done
  printf '\n'

  # ── Unsafe L2 block ────────────────────────────────────────────────────────
  printf "  ${BOLD}%-16s${RESET}" "Unsafe L2"
  for i in 0 1 2; do
    local blk="${unsafes[$i]}"
    if [ "${leaders[$i]}" = "true" ]; then
      printf "${YELLOW}$(cell "   #$blk" $W)${RESET}"
    else
      printf "$(cell "   #$blk" $W)"
    fi
  done
  printf '\n'

  # ── P2P Peers ──────────────────────────────────────────────────────────────
  printf "  ${BOLD}%-16s${RESET}" "P2P Peers"
  for i in 0 1 2; do
    local p="${peerss[$i]}"
    if [ "$p" = "0" ] || [ "$p" = "?" ]; then
      printf "${RED}$(cell "   $p" $W)${RESET}"
    else
      printf "${GREEN}$(cell "   $p" $W)${RESET}"
    fi
  done
  printf '\n'

  # ── Clear any leftover lines below the table ────────────────────────────────
  printf '\033[J'
}

# Initial clear + hide cursor
tput civis 2>/dev/null || true
clear

while true; do
  # Fetch all three nodes in parallel
  for i in 0 1 2; do
    fetch_node "${CONDUCTOR_PORTS[$i]}" "${CL_PORTS[$i]}" "${TMPDIR_DATA}/$i" &
  done
  wait

  draw
  sleep "$REFRESH"
done
