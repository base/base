#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

RPC_URL="${1:-$L2_INGRESS_RPC_URL}"
AUDIT_RPC_URL="${2:-http://localhost:${AUDIT_RPC_PORT:-9100}}"
PK="${3:-$ANVIL_ACCOUNT_5_KEY}"
TO="${4:-$ANVIL_ACCOUNT_6_ADDR}"

echo "=== Transaction Events Smoke ==="
echo "Sending L2 tx through ingress..."
from="$(cast wallet address --private-key "$PK")"
nonce="$(cast nonce --block pending --rpc-url "$RPC_URL" "$from")"
tx_hash="$(
  cast send --async --private-key "$PK" --rpc-url "$RPC_URL" --nonce "$nonce" "$TO" --value 0.0001ether \
    | tail -n 1
)"
echo "TX: ${tx_hash}"

payload="$(
  jq -nc --arg tx_hash "$tx_hash" '{
    jsonrpc: "2.0",
    method: "base_getTransactionEventsByHash",
    params: [$tx_hash, 100],
    id: 1
  }'
)"

echo "Waiting for Vector -> audit-archiver -> Postgres..."
last_response=""
for attempt in $(seq 1 60); do
  response="$(curl -fsS -H 'content-type: application/json' --data "$payload" "$AUDIT_RPC_URL" 2>/dev/null || true)"
  if [ -n "$response" ]; then
    last_response="$response"
  fi

  count="$(jq -r 'if (.result | type) == "array" then (.result | length) else 0 end' <<<"$response" 2>/dev/null || echo 0)"
  if [ "$count" -gt 0 ]; then
    if jq -e '
      def has_event($producer; $event_types):
        any(.result[]; .schema_version == "transaction-event/v1"
          and .producer == $producer
          and (.event_type as $event_type | $event_types | index($event_type)));

      has_event("ingress-rpc"; ["INGRESS_RECEIVED"])
      and has_event("base-routing/proxyd"; ["PROXY_RECEIVED"])
      and has_event("base-reth-node"; [
        "TXPOOL_PENDING",
        "TXPOOL_QUEUED",
        "TXPOOL_BUILDER_FORWARD_ATTEMPT",
        "TXPOOL_BUILDER_FORWARD_SUCCESS"
      ])
      and has_event("base-builder"; [
        "BUILDER_CONSIDERED",
        "BUILDER_ACCEPTED",
        "BUILDER_INCLUDED"
      ])
    ' <<<"$response" >/dev/null; then
      echo "Observed ${count} persisted transaction event(s) for ${tx_hash}"
      jq '.result[] | {event_time, producer, event_type, tx_hash, data}' <<<"$response"
      exit 0
    fi
  fi

  if [ "$attempt" = 60 ]; then
    echo "Timed out waiting for ingress, proxyd, txpool, and builder transaction events for ${tx_hash}" >&2
    if [ -n "$last_response" ]; then
      echo "$last_response" | jq . >&2 || echo "$last_response" >&2
    fi
    exit 1
  fi
  sleep 1
done
