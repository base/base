#!/usr/bin/env bash
# THROWAWAY one-off measurement kit — P2P block-latency measurement.
#
# Launches a base-consensus "observer" node that stays at the tip of Base
# mainnet, joins the CL gossip mesh, and appends one CSV row per received block
# via the two NEW latency flags. Everything except those two flags is grounded
# in the real, existing base-consensus CLI (see file:line refs in README.md).
#
# Required env:
#   REGION            region label, e.g. us-east (fail-fast if unset)
#   L1_ETH_RPC        L1 execution RPC URL         (--l1-eth-rpc)
#   L1_BEACON         L1 beacon API URL            (--l1-beacon)
#   L2_ENGINE_RPC     L2 engine (auth) RPC URL     (--l2-engine-rpc)
#   L2_JWT_PATH       path to hex JWT secret file  (--l2.jwt-secret)
# Optional env:
#   LOG_PATH          CSV output (default /var/lib/base-observer/latency-<region>.csv)
#   CHAIN             L2 chain id/name (default 8453 = Base mainnet)
#   BASE_CONSENSUS_BIN  path to binary (default: base-consensus on PATH)
#   ADVERTISE_IP      public IP to advertise to peers (recommended on cloud VMs)
#   P2P_TCP_PORT      default 9222
#   P2P_UDP_PORT      default 9223
#   BOOTSTORE_DIR     bootstore dir (default /var/lib/base-observer/bootstore)
#   BOOTNODES         optional comma-separated bootnode ENRs/records
set -euo pipefail

# --- fail fast on required inputs -------------------------------------------
if [[ -z "${REGION:-}" ]]; then
  echo "ERROR: REGION is unset. Set it to a region label, e.g. REGION=us-east." >&2
  exit 1
fi

: "${L1_ETH_RPC:?ERROR: L1_ETH_RPC is unset (L1 execution RPC URL)}"
: "${L1_BEACON:?ERROR: L1_BEACON is unset (L1 beacon API URL)}"
: "${L2_ENGINE_RPC:?ERROR: L2_ENGINE_RPC is unset (L2 engine auth RPC URL)}"
: "${L2_JWT_PATH:?ERROR: L2_JWT_PATH is unset (path to hex JWT secret file)}"

CHAIN="${CHAIN:-8453}"
BASE_CONSENSUS_BIN="${BASE_CONSENSUS_BIN:-base-consensus}"
LOG_PATH="${LOG_PATH:-/var/lib/base-observer/latency-${REGION}.csv}"
BOOTSTORE_DIR="${BOOTSTORE_DIR:-/var/lib/base-observer/bootstore}"
P2P_TCP_PORT="${P2P_TCP_PORT:-9222}"
P2P_UDP_PORT="${P2P_UDP_PORT:-9223}"

# Ensure output/bootstore directories exist (snap-sync persists peers here).
mkdir -p "$(dirname "${LOG_PATH}")" "${BOOTSTORE_DIR}"

# --- assemble the run command -----------------------------------------------
# REAL, existing flags (grounded — see README "Confirmed real flags"):
args=(
  node
  --chain "${CHAIN}"                 # chain.rs:24  (env BASE_NODE_NETWORK)
  --l1-eth-rpc "${L1_ETH_RPC}"       # l1.rs:11
  --l1-beacon "${L1_BEACON}"         # l1.rs:23
  --l2-engine-rpc "${L2_ENGINE_RPC}" # l2.rs:17
  --l2.jwt-secret "${L2_JWT_PATH}"   # l2.rs:21
  --p2p.listen.tcp "${P2P_TCP_PORT}" # p2p.rs:106 (default 9222)
  --p2p.listen.udp "${P2P_UDP_PORT}" # p2p.rs:109 (default 9223)
  --p2p.bootstore "${BOOTSTORE_DIR}" # p2p.rs:207  (persist discovered peers)
  --metrics.enabled                  # macros.rs:38 (metrics on :9090 for mesh sanity)
)

# Advertise a static public IP if provided (recommended behind cloud NAT).
if [[ -n "${ADVERTISE_IP:-}" ]]; then
  args+=(--p2p.advertise.ip "${ADVERTISE_IP}")  # p2p.rs:89
fi

# Optional explicit bootnodes; otherwise built-in defaults for the chain are used.
if [[ -n "${BOOTNODES:-}" ]]; then
  args+=(--p2p.bootnodes "${BOOTNODES}")         # p2p.rs:235
fi

# ---------------------------------------------------------------------------
# NEW latency flags (being ADDED to base-consensus for this measurement).
# TODO: confirm exact flag spelling + env names against
#       crates/consensus/cli/src/p2p.rs once they land; they do not yet exist.
args+=(
  --p2p.latency.log "${LOG_PATH}"    # NEW (env BASE_NODE_P2P_LATENCY_LOG)
  --p2p.latency.region "${REGION}"   # NEW (env BASE_NODE_P2P_LATENCY_REGION)
)
# ---------------------------------------------------------------------------

echo "==> Launching base-consensus observer"
echo "    region=${REGION} chain=${CHAIN} log=${LOG_PATH}"
exec "${BASE_CONSENSUS_BIN}" "${args[@]}"
