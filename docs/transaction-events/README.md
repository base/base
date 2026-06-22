# Transaction Event Journal

This document defines `transaction-event/v1`, the shared business event journal
contract for Base transaction observability. Producers write newline-delimited
JSON records to a dedicated file. Stdout/stderr logs continue through the normal
Kubernetes Datadog path and must not be reused for this journal.

Vector tails these same JSONL files and ships newline-delimited event records to
`audit-archiver`. The audit HTTP ingest endpoint is collector-facing and expects
one event JSON object per line, not a wrapped JSON batch.

## Configuration Fields

Rust producers should expose these config fields directly or with a
producer-specific prefix:

| Field | Type | Meaning |
| --- | --- | --- |
| `enabled` | boolean | Enables transaction event journal writes. |
| `file_path` | string | Dedicated JSONL file path tailed by Vector. |
| `queue_capacity` | integer | Bounded in-process event queue size. Producers drop on backpressure instead of blocking transaction serving paths. |
| `required` | boolean | If true, fail service initialization when the file writer cannot open. Runtime write failures remain observable and non-fatal. |
| `producer` | string | One of the producer identities below. |
| `network` | string | Network label, for example `base-mainnet` or `base-sepolia`. |

For Go/proxyd, mirror the same names in TOML:

```toml
[transaction_events]
enabled = true
file_path = "/var/log/base/transaction-events.jsonl"
queue_capacity = 16384
required = false
producer = "base-routing/proxyd"
network = "base-mainnet"
```

## Envelope

Each line is one JSON object:

```json
{
  "schema_version": "transaction-event/v1",
  "event_id": "0x7d5c4f...",
  "event_time": "2026-06-02T00:00:00.000000000Z",
  "producer": "base-reth-node",
  "event_type": "TXPOOL_PENDING",
  "network": "base-mainnet",
  "tx_hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
  "block_hash": null,
  "block_number": null,
  "payload_id": null,
  "request_id": null,
  "data": {
    "pool": "pending"
  }
}
```

Required fields:

- `schema_version`
- `event_id`
- `event_time`
- `producer`
- `event_type`

At least one join key should normally be present: `tx_hash`,
`block_hash`/`block_number`, or `payload_id`. `request_id` is optional but useful
for proxy and ingress correlation. Producers should not emit transaction journal
events for aggregate operational conditions that cannot be tied to one of these
join keys. For example, broadcast lag is reported through logs and metrics
because the receiver only knows a skipped count, while
`INGRESS_METERING_SEND_DROPPED` is emitted once per dropped transaction only
when ingress still has the original `tx_hash`.

Producer-specific fields belong in `data`. Do not write raw transaction bytes,
calldata, full request bodies, API keys, secrets, private keys, bearer tokens,
authorization headers, raw forwarding headers, or raw client IP forwarding
chains.

Collector sidecars may add deployment-specific source metadata under
`data.observability_source` before shipping events to `audit-archiver`.
`audit-archiver` stores this object with the rest of `data`, but the shared
contract does not validate its shape.

The Rust `TransactionEvent::validate` helper rejects the wrong schema version,
empty `event_id`, and a small exact unsafe `data` key denylist such as `raw_tx`,
`calldata`, `request_body`, `authorization`, `api_key`, `headers`, and
`x-forwarded-for`. Vector collector pipelines should reject broader
case/delimiter variants such as `rawTransaction`, `requestBody`, `secret_key`,
and `privateKey` before ingest.

## Local Devnet Verification

The ingress devnet stack runs a local Postgres, Vector shipper, and
Postgres-backed `audit-archiver` ingest path:

```bash
just devnet ingress
just devnet tx-observability-smoke
```

Set `BASE_ROUTING_CONTEXT` to a local `protocols/base-routing` checkout when
testing proxyd transaction events before that implementation has landed in the
default proxyd image:

```bash
BASE_ROUTING_CONTEXT=/path/to/base-routing just devnet ingress
just devnet tx-observability-smoke
```

The smoke test sends one transaction through ingress, waits for Vector to ship
JSONL events from ingress, proxyd, txpool tracing, and builder producers, and
verifies `audit-archiver` can read the persisted events back from Postgres by
transaction hash.

For local Vector health, alert or inspect `component_discarded_events_total`.
`parse_transaction_events` drops malformed JSONL lines, and
`validate_transaction_events` drops parsed events with unsafe `data` keys.

## Producer Values

- `base-reth-node`
- `base-builder`
- `ingress-rpc`
- `base-routing/proxyd`

## Txpool Tracing Example

`base-reth-node` txpool tracing can emit the existing live LRU events to the
durable journal when `--enable-transaction-event-journal` and
`--transaction-event-journal-path` are set:

```json
{"schema_version":"transaction-event/v1","event_id":"0x4d6d...","event_time":"2026-06-02T00:00:00Z","producer":"base-reth-node","event_type":"TXPOOL_PENDING","network":"base-mainnet","tx_hash":"0x1111111111111111111111111111111111111111111111111111111111111111","block_hash":null,"block_number":null,"payload_id":null,"request_id":null,"data":{"event_source":"txpool-tracing","txpool_event":"pending","event_index":0,"node_role":"mempool","pool":"pending"}}
```

## Event Vocabulary

Edge/proxy:

- `PROXY_RECEIVED`
- `PROXY_REJECTED`
- `PROXY_VALIDATION_ACCEPTED`
- `PROXY_VALIDATION_REJECTED`
- `PROXY_ROUTED_TO_BACKEND`
- `PROXY_BACKEND_SUCCESS`
- `PROXY_BACKEND_FAILURE`
- `PROXY_INGRESS_RPC_ATTEMPT`
- `PROXY_INGRESS_RPC_SUCCESS`
- `PROXY_INGRESS_RPC_FAILURE`

Ingress/audit:

- `INGRESS_RECEIVED`
- `SIMULATION_STARTED`
- `SIMULATION_SUCCEEDED`
- `SIMULATION_FAILED`
- `INGRESS_METERING_SEND_ATTEMPT`
- `INGRESS_METERING_SEND_SUCCESS`
- `INGRESS_METERING_SEND_FAILURE`
- `INGRESS_METERING_SEND_DROPPED`

Mempool/node:

- `TXPOOL_PENDING`
- `TXPOOL_QUEUED`
- `TXPOOL_PENDING_TO_QUEUED`
- `TXPOOL_QUEUED_TO_PENDING`
- `TXPOOL_DROPPED`
- `TXPOOL_REPLACED`
- `TXPOOL_TRACKING_OVERFLOWED`
- `TXPOOL_BLOCK_INCLUDED`
- `TXPOOL_FLASHBLOCK_INCLUDED`

Forwarding:

- `TXPOOL_BUILDER_FORWARD_ATTEMPT`
- `TXPOOL_BUILDER_FORWARD_SUCCESS`
- `TXPOOL_BUILDER_FORWARD_FAILURE`
- `TXPOOL_BUILDER_FORWARD_DROPPED`
- `TXPOOL_VALIDATED_INSERT_ACCEPTED`
- `TXPOOL_VALIDATED_INSERT_REJECTED`

`TXPOOL_BUILDER_FORWARD_DROPPED` is emitted only for transaction-scoped drops
where the forwarding task still knows the `tx_hash`, such as final RPC failure
after retries. Broadcast lag is intentionally excluded from the transaction
journal and remains visible through logs and metrics.

Builder:

- `BUILDER_CONSIDERED`
- `BUILDER_ACCEPTED`
- `BUILDER_REJECTED`
- `BUILDER_INCLUDED`
- `BUILDER_PAYLOAD_FINALIZED`
- `BUILDER_FLASHBLOCK_STARTED`
- `BUILDER_FLASHBLOCK_PUBLISHED`
- `BUILDER_FLASHBLOCK_BUILD_STOPPED`

Builder caveat: `BUILDER_CONSIDERED`, `BUILDER_ACCEPTED`, and
`BUILDER_REJECTED` are emitted per payload-building attempt and include
`payload_id`, `block_number`, and `flashblock_index` when applicable. The same
transaction can therefore produce multiple decision events across flashblocks.
`BUILDER_INCLUDED` is emitted when the builder finalizes the payload it can
serve via `engine_getPayload` and includes
`data.inclusion_signal = "builder_finalized_payload"`. The payload loop emits
nonce and validation skips as `BUILDER_REJECTED` rather than inventing
replacement relationships. `BUILDER_PAYLOAD_FINALIZED` is emitted once for each
built payload and links `payload_id` to the builder's block hash and number even
when the payload contains no user transactions. It includes `data.parent_hash`,
`data.transaction_count`, `data.gas_used`, `data.gas_limit`, and
`data.timestamp`.
`BUILDER_FLASHBLOCK_STARTED`, `BUILDER_FLASHBLOCK_PUBLISHED`, and
`BUILDER_FLASHBLOCK_BUILD_STOPPED` are payload/flashblock-scoped events. They
include top-level `payload_id` and `block_number`, plus `data.parent_hash`,
`data.flashblock_index`, and `data.target_flashblock_count`. Published events
also include top-level `block_hash`, `data.transaction_count`, `data.byte_size`,
and `data.build_duration_ms`. Build-stopped events use `data.reason` to
distinguish control-flow stops such as payload resolution winning before
publish.

Canonicality caveat: builder events are local payload construction signals, not
canonical-chain or consensus-finality observations. A builder event with
`block_hash` means the builder computed or published that payload shape; it does
not by itself prove that the block later became canonical. Canonical block
history should be linked through canonical-state observers such as txpool
tracing by matching `block_hash`, `block_number`, and transaction hashes.

## Event ID Guidance

Use deterministic `event_id` values wherever the source has stable inputs.
Recommended components:

- `producer`
- `event_type`
- source timestamp bucket or source sequence
- `tx_hash`
- `request_id`
- backend/node identifier when applicable
- attempt index when applicable

If a source cannot produce an exactly deterministic ID, document why in the
producer implementation and include enough fields in `data` for
`audit-archiver` to enforce database-side uniqueness.

## proxyd Examples

Received raw transaction request:

```json
{
  "schema_version": "transaction-event/v1",
  "event_id": "0x1f3f...",
  "event_time": "2026-06-02T00:00:00.000000000Z",
  "producer": "base-routing/proxyd",
  "event_type": "PROXY_RECEIVED",
  "network": "base-mainnet",
  "tx_hash": "0x2222222222222222222222222222222222222222222222222222222222222222",
  "block_hash": null,
  "block_number": null,
  "payload_id": null,
  "request_id": "req-abc",
  "data": {
    "rpc_method": "eth_sendRawTransaction"
  }
}
```

Validation rejection:

```json
{
  "schema_version": "transaction-event/v1",
  "event_id": "0x2a4b...",
  "event_time": "2026-06-02T00:00:00.000000000Z",
  "producer": "base-routing/proxyd",
  "event_type": "PROXY_VALIDATION_REJECTED",
  "network": "base-mainnet",
  "tx_hash": "0x2222222222222222222222222222222222222222222222222222222222222222",
  "block_hash": null,
  "block_number": null,
  "payload_id": null,
  "request_id": "req-abc",
  "data": {
    "rpc_method": "eth_sendRawTransaction",
    "validation_service": "tx-validation",
    "fail_open": false
  }
}
```

Routed to node:

```json
{
  "schema_version": "transaction-event/v1",
  "event_id": "0x3b5c...",
  "event_time": "2026-06-02T00:00:00.000000000Z",
  "producer": "base-routing/proxyd",
  "event_type": "PROXY_ROUTED_TO_BACKEND",
  "network": "base-mainnet",
  "tx_hash": "0x2222222222222222222222222222222222222222222222222222222222222222",
  "block_hash": null,
  "block_number": null,
  "payload_id": null,
  "request_id": "req-abc",
  "data": {
    "backend": "reth-mainnet-0",
    "attempt_index": 0
  }
}
```
