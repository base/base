#!/bin/bash

# Run the OP Proposer. Note: The DB is persisted across restarts with a Docker volume at the path
# `/usr/local/bin/dbdata`.

# Currently, configured to generate a proof once per minute.

/usr/local/bin/op-proposer \
    --poll-interval=${POLL_INTERVAL:-60s} \
    --rollup-rpc=${L2_NODE_RPC} \
    --l2oo-address=${L2OO_ADDRESS} \
    --private-key=${PRIVATE_KEY} \
    --l1-eth-rpc=${L1_RPC} \
    --beacon-rpc=${L1_BEACON_RPC} \
    --l2-chain-id=${L2_CHAIN_ID} \
    --max-concurrent-proof-requests=${MAX_CONCURRENT_PROOF_REQUESTS:-40} \
    --db-path=/usr/local/bin/dbdata/proofs.db \
    --op-succinct-server-url=${OP_SUCCINCT_SERVER_URL:-0.0.0.0:3000} \
    --max-block-range-per-span-proof=${MAX_BLOCK_RANGE_PER_SPAN_PROOF:-20} \
    --use-cached-db=${USE_CACHED_DB:-false}
