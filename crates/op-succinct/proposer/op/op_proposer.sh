#!/bin/bash

# Run the OP Proposer. Note: The DB is persisted across restarts with a Docker volume at the path
# `/usr/local/bin/dbdata`.

# Currently, configured to generate a proof once per minute.

/usr/local/bin/op-proposer \
    --poll-interval=${POLL_INTERVAL:-20s} \
    --rollup-rpc=${L2_NODE_RPC} \
    --l2oo-address=${L2OO_ADDRESS} \
    --private-key=${PRIVATE_KEY} \
    --l1-eth-rpc=${L1_RPC} \
    --beacon-rpc=${L1_BEACON_RPC} \
    --max-concurrent-proof-requests=${MAX_CONCURRENT_PROOF_REQUESTS:-10} \
    --db-path=${DB_PATH:-/usr/local/bin/dbdata} \
    --op-succinct-server-url=${OP_SUCCINCT_SERVER_URL:-http://op-succinct-server:3000} \
    --max-block-range-per-span-proof=${MAX_BLOCK_RANGE_PER_SPAN_PROOF:-20} \
    --use-cached-db=${USE_CACHED_DB:-false} \
    --metrics.enabled=${METRICS_ENABLED:-true} \
    --metrics.port=${METRICS_PORT:-7300} \
    ${SLACK_TOKEN:+--slack-token=${SLACK_TOKEN}} \  # Pass the Slack token if it is set.
