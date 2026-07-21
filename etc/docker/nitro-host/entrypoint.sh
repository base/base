#!/bin/bash
set -eux

: "${VSOCK_CID:?required}"
: "${L1_ETH_URL:?required}"
: "${L1_BEACON_URL:?required}"
: "${L2_ETH_URL:?required}"
: "${L2_NODE_URL:?required}"
: "${L2_CHAIN_ID:?required}"
: "${PROVER_SERVICE_ENDPOINT:?required}"

ADDITIONAL_ARGS=()
if [ -n "${LISTEN_ADDR:-}" ]; then
    ADDITIONAL_ARGS+=(--listen-addr="$LISTEN_ADDR")
fi
if [ -n "${TEE_PROVER_REGISTRY_ADDRESS:-}" ]; then
    ADDITIONAL_ARGS+=(--tee-prover-registry-address="$TEE_PROVER_REGISTRY_ADDRESS")
fi

exec ./base-prover-nitro-host \
    server \
    --l1-eth-url "$L1_ETH_URL" \
    --l1-beacon-url "$L1_BEACON_URL" \
    --l2-eth-url "$L2_ETH_URL" \
    --l2-node-url "$L2_NODE_URL" \
    --l2-chain-id "$L2_CHAIN_ID" \
    --prover-service-endpoint "$PROVER_SERVICE_ENDPOINT" \
    --vsock-cid "$VSOCK_CID" \
    --enable-experimental-witness-endpoint \
    "${ADDITIONAL_ARGS[@]}"
