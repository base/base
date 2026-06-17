#!/usr/bin/env bash
set -euo pipefail

# setup-proposer.sh — entrypoint shim for the L3 dev-multiproof base-proposer service.
#
# The DisputeGameFactory and AnchorStateRegistry addresses are only known after the L1
# contract deploy, so they cannot be baked into a committed env file. This shim reads them
# from the shared l1-addresses.json artifact, exports them as the proposer's address env
# vars, then exec's the binary. All other config arrives via env (set in docker-compose.yml).
#
# Runs inside the base-proposer image (debian-slim, no jq), so addresses are parsed with sed
# from the pretty-printed JSON map written by extract-artifacts.sh.

ADDRESSES_FILE="${ADDRESSES_FILE:-/devnet/l2/configs/l1-addresses.json}"

if [ ! -f "$ADDRESSES_FILE" ]; then
  echo "ERROR: l1 addresses file not found: $ADDRESSES_FILE" >&2
  exit 1
fi

# Extract a single 0x-prefixed address for a PascalCase key from the JSON map.
read_addr() {
  local key="$1"
  sed -n "s/.*\"${key}\": *\"\(0x[0-9a-fA-F]*\)\".*/\1/p" "$ADDRESSES_FILE" | head -n1
}

DISPUTE_GAME_FACTORY_ADDR="$(read_addr DisputeGameFactoryProxy)"
ANCHOR_STATE_REGISTRY_ADDR="$(read_addr AnchorStateRegistryProxy)"

for pair in \
  "DisputeGameFactoryProxy=$DISPUTE_GAME_FACTORY_ADDR" \
  "AnchorStateRegistryProxy=$ANCHOR_STATE_REGISTRY_ADDR"; do
  name="${pair%%=*}"
  value="${pair#*=}"
  if ! [[ "$value" =~ ^0x[0-9a-fA-F]{40}$ ]]; then
    echo "ERROR: could not read a valid $name address from $ADDRESSES_FILE (got: '$value')" >&2
    exit 1
  fi
done

export BASE_PROPOSER_DISPUTE_GAME_FACTORY_ADDR="$DISPUTE_GAME_FACTORY_ADDR"
export BASE_PROPOSER_ANCHOR_STATE_REGISTRY_ADDR="$ANCHOR_STATE_REGISTRY_ADDR"

echo "proposer: DisputeGameFactory=$DISPUTE_GAME_FACTORY_ADDR AnchorStateRegistry=$ANCHOR_STATE_REGISTRY_ADDR"

exec /app/base-proposer
