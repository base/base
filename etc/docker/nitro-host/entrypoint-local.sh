#!/bin/bash
set -eux

: "${LISTEN_ADDR:?required}"
: "${L1_ETH_URL:?required}"
: "${L2_ETH_URL:?required}"
: "${L2_CHAIN_ID:?required}"

ADDITIONAL_ARGS=()

# Default to calldata-only mode for appchain (L3) use.
# Set L1_BEACON_URL to override with blob-backed proving instead.
if [ -n "${L1_BEACON_URL:-}" ]; then
    ADDITIONAL_ARGS+=(--l1-beacon-url="$L1_BEACON_URL")
else
    ADDITIONAL_ARGS+=(--l1-calldata-only)
fi

if [ -n "${TEE_PROVER_REGISTRY_ADDRESS:-}" ]; then
    ADDITIONAL_ARGS+=(--tee-prover-registry-address="$TEE_PROVER_REGISTRY_ADDRESS")
fi

if [ -n "${LOCAL_ENCLAVE_COUNT:-}" ]; then
    ADDITIONAL_ARGS+=(--local-enclave-count="$LOCAL_ENCLAVE_COUNT")
fi

exec ./base-prover-nitro-host \
    local \
    --l1-eth-url "$L1_ETH_URL" \
    --l2-eth-url "$L2_ETH_URL" \
    --l2-chain-id "$L2_CHAIN_ID" \
    --listen-addr "$LISTEN_ADDR" \
    --enable-experimental-witness-endpoint \
    "${ADDITIONAL_ARGS[@]}"
