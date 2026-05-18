#!/bin/bash
set -eux

: "${VSOCK_CID:?required}"
: "${LISTEN_ADDR:?required}"
: "${L1_ETH_URL:?required}"
: "${L1_BEACON_URL:?required}"
: "${L2_ETH_URL:?required}"
: "${L2_CHAIN_ID:?required}"
: "${PROOF_REQUEST_TIMEOUT_SECS:=3600}"

ADDITIONAL_ARGS=()
if [ -n "${TEE_PROVER_REGISTRY_ADDRESS:-}" ]; then
    ADDITIONAL_ARGS+=(--tee-prover-registry-address="$TEE_PROVER_REGISTRY_ADDRESS")
fi

exec ./base-prover-nitro-host \
    server \
    --l1-eth-url "$L1_ETH_URL" \
    --l1-beacon-url "$L1_BEACON_URL" \
    --l2-eth-url "$L2_ETH_URL" \
    --l2-chain-id "$L2_CHAIN_ID" \
    --listen-addr "$LISTEN_ADDR" \
    --vsock-cid "$VSOCK_CID" \
    --proof-request-timeout-secs "$PROOF_REQUEST_TIMEOUT_SECS" \
    --enable-experimental-witness-endpoint \
    "${ADDITIONAL_ARGS[@]}"
