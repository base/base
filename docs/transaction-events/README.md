# Transaction Observability Event Journal

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
| `flush_interval` | duration | Background file flush interval. |
| `required` | boolean | If true, fail service initialization when the file writer cannot open. Runtime write failures remain observable and non-fatal. |
| `producer` | string | One of the producer identities below. |
| `network` | string | Network label, for example `base-mainnet` or `base-sepolia`. |

For Go/proxyd, mirror the same names in TOML:

```toml
[transaction_events]
enabled = true
file_path = "/var/log/base/transaction-events.jsonl"
queue_capacity = 16384
flush_interval = "1s"
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
  "event_type": "PENDING",
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
for proxy and ingress correlation.

Producer-specific fields belong in `data`. Do not write raw transaction bytes,
calldata, full request bodies, API keys, secrets, authorization headers, raw
forwarding headers, or raw client IP forwarding chains.

Collector sidecars may add deployment-specific source metadata under
`data.observability_source` before shipping events to `audit-archiver`.
`audit-archiver` stores this object with the rest of `data`, but the shared
contract does not validate its shape.

The Rust `TransactionEvent::validate` helper rejects the wrong schema version,
empty `event_id`, and known unsafe `data` keys such as `raw_tx`, `calldata`,
`authorization`, `api_key`, `headers`, and `x-forwarded-for`.

## Local Devnet Verification

The ingress devnet stack runs a local Postgres, Vector shipper, and
Postgres-backed `audit-archiver` ingest path:

```bash
just devnet ingress
just devnet tx-observability-smoke
```

The smoke test sends one transaction through ingress, waits for Vector to ship
the producer JSONL event, and verifies `audit-archiver` can read the persisted
event back from Postgres by transaction hash.

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
{"schema_version":"transaction-event/v1","event_id":"0x4d6d...","event_time":"2026-06-02T00:00:00Z","producer":"base-reth-node","event_type":"PENDING","network":"base-mainnet","tx_hash":"0x1111111111111111111111111111111111111111111111111111111111111111","block_hash":null,"block_number":null,"payload_id":null,"request_id":null,"data":{"event_source":"txpool-tracing","txpool_event":"pending","event_index":0,"node_role":"mempool","pool":"pending"}}
```

## Event Vocabulary

Edge/proxy:

- `PROXY_RECEIVED`
- `PROXY_REJECTED`
- `PROXY_VALIDATION_ACCEPTED`
- `PROXY_VALIDATION_REJECTED`
- `ROUTED_TO_NODE`
- `NODE_ACCEPTED`
- `NODE_REJECTED`
- `INGRESS_RPC_FORWARD_ATTEMPT`
- `INGRESS_RPC_FORWARD_SUCCESS`
- `INGRESS_RPC_FORWARD_FAILURE`

Ingress/audit:

- `INGRESS_RECEIVED`
- `SIMULATION_STARTED`
- `SIMULATION_ACCEPTED`
- `SIMULATION_REJECTED`

Mempool/node:

- `PENDING`
- `QUEUED`
- `PENDING_TO_QUEUED`
- `QUEUED_TO_PENDING`
- `DROPPED`
- `REPLACED`
- `OVERFLOWED`
- `INCLUDED`
- `FLASHBLOCK_INCLUDED`

Forwarding:

- `FORWARD_ATTEMPT`
- `FORWARD_ACK`
- `FORWARD_NACK`

Builder:

- `BUILDER_CONSIDERED`
- `BUILDER_ACCEPTED`
- `BUILDER_REJECTED`
- `BUILDER_INCLUDED`

Builder caveat: `BUILDER_CONSIDERED`, `BUILDER_ACCEPTED`, and
`BUILDER_REJECTED` are emitted per payload-building attempt and include
`payload_id`, `block_number`, and `flashblock_index` when applicable. The same
transaction can therefore produce multiple decision events across flashblocks.
`BUILDER_INCLUDED` is emitted when the builder finalizes the payload it can
serve via `engine_getPayload`; the builder does not independently observe later
canonical chain inclusion, so these events include
`data.inclusion_signal = "builder_finalized_payload"` and
`data.canonicality = "not_observed_by_builder"`. The payload loop emits nonce
and validation skips as `BUILDER_REJECTED` rather than inventing replacement
relationships.

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
  "event_type": "ROUTED_TO_NODE",
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
