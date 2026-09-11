# Transaction Event Journal

This document defines `transaction-event/v1`, the shared business event journal
contract for Base transaction observability. Producers write newline-delimited
JSON records to a dedicated file. Stdout/stderr logs continue through the normal
Kubernetes Datadog path and must not be reused for this journal.

Vector tails these same JSONL files and ships newline-delimited event records to
`audit-archiver`. The audit HTTP ingest endpoint is collector-facing and expects
one event JSON object per line, not a wrapped JSON batch.

## Postgres Retention

`audit-archiver` stores events in Postgres for operational queries. Postgres is
not the long-term archive. A background worker deletes rows by event type:
high-volume builder-decision events default to 3 days, ingress and
forwarding events default to 7 days, and failures, drops, inclusion, and
block events default to 30 days. Autovacuum reclaims the resulting table
bloat. `TXPOOL_SEND_RAW_TRANSACTION_VALIDITY` uses the same warm window as
`TXPOOL_SEND_RAW_TRANSACTION`. `BUILDER_DEFERRED` and `BUILDER_EXPIRED` use the
same hot window as the other per-attempt builder decisions; deferral can fire
once per block for a parked validity transaction.

## Configuration Fields

Rust producers should expose these config fields directly or with a
producer-specific prefix:

| Field | Type | Meaning |
| --- | --- | --- |
| `enabled` | boolean | Enables transaction event journal writes. |
| `file_path` | string | Dedicated JSONL file path tailed by Vector. |
| `queue_capacity` | integer | Bounded in-process event queue size. Producers drop on backpressure instead of blocking transaction serving paths. |
| `max_file_bytes` | integer | Maximum size of the active JSONL segment before it is renamed and a new segment is opened. |
| `max_files` | integer | Maximum number of JSONL segments retained, including the active segment. |
| `required` | boolean | If true, fail service initialization when the file writer cannot open. Runtime write failures remain observable and non-fatal. |
| `producer` | string | One of the producer identities below. |
| `network` | string | Network label, for example `base-mainnet` or `base-sepolia`. |

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
for ingress correlation. Producers should not emit transaction journal
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

Core devnet (`just devnet up` / `just devnet up-single`) enables durable
transaction event journals on `base-client` and `base-builder`, writing JSONL
under `.devnet/transaction-events/`. The ingress overlay adds the collection
pipeline (Vector, Postgres, `audit-archiver`); it does not own node journal config.

```bash
just devnet ingress
just devnet tx-observability-smoke
```

The smoke test sends one transaction directly to the client node, waits for Vector
to ship JSONL events from txpool tracing and builder producers, and
verifies `audit-archiver` can read the persisted events back from Postgres by
transaction hash.

For local Vector health, alert or inspect `component_discarded_events_total`.
`parse_transaction_events` drops malformed JSONL lines, and
`validate_transaction_events` drops parsed events with unsafe `data` keys.

## Producer Values

- `base-reth-node` (stable event producer identifier for execution inside `base`)
- `base-builder`
- `ingress-rpc`

## Txpool Tracing Example

The execution service in `base` can emit the existing live LRU events to the
durable journal when `--enable-transaction-event-journal` and
`--transaction-event-journal-path` are set:

```json
{"schema_version":"transaction-event/v1","event_id":"0x4d6d...","event_time":"2026-06-02T00:00:00Z","producer":"base-reth-node","event_type":"TXPOOL_PENDING","network":"base-mainnet","tx_hash":"0x1111111111111111111111111111111111111111111111111111111111111111","block_hash":null,"block_number":null,"payload_id":null,"request_id":null,"data":{"event_source":"txpool-tracing","txpool_event":"pending","event_index":0,"node_role":"mempool","pool":"pending"}}
```

## Event Vocabulary

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
- `TXPOOL_SEND_RAW_TRANSACTION`
- `TXPOOL_SEND_RAW_TRANSACTION_VALIDITY`

`TXPOOL_SEND_RAW_TRANSACTION` and `TXPOOL_SEND_RAW_TRANSACTION_VALIDITY` are
one-time RPC-admission events, one per unique submit path. They fire after the
transaction is decoded and before sequencer forwarding or pool insertion.
`tx_hash` is the join key. They are distinct from `TXPOOL_PENDING` /
`TXPOOL_QUEUED`, which record later subpool membership.
`TXPOOL_SEND_RAW_TRANSACTION_VALIDITY` is the **only** event that records
`data.validity_predicates` (the serialized `balance`, `storage`,
and `block_number` list). Downstream lifecycle events —
including `BUILDER_DEFERRED`, `BUILDER_EXPIRED`, `BUILDER_ACCEPTED`, and
`BUILDER_INCLUDED` — must not repeat that list; join them back by `tx_hash`. A
replacement is a fresh admission with its own `tx_hash` and/or predicate list;
`TXPOOL_REPLACED.replacement_hash` links the outgoing transaction to the
incoming one. `base_insertValidatedTransaction` uses
`TXPOOL_VALIDATED_INSERT_ACCEPTED` / `TXPOOL_VALIDATED_INSERT_REJECTED`.

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
- `BUILDER_DEFERRED`
- `BUILDER_EXPIRED`
- `BUILDER_INCLUDED`
- `BUILDER_PAYLOAD_FINALIZED`

Builder caveat: `BUILDER_CONSIDERED`, `BUILDER_ACCEPTED`,
`BUILDER_REJECTED`, `BUILDER_DEFERRED`, and `BUILDER_EXPIRED` are emitted per
payload-building attempt and include `payload_id` and `block_number`. The same transaction can therefore produce
multiple decision events across blocks. `BUILDER_DEFERRED` is emitted each
time the builder moves a transaction from the selection queue into the parking
lot, including after a promote-and-repark in the same block. Reindexing an
already-parked transaction when its blocker changes does not emit another
`BUILDER_DEFERRED`. `BUILDER_EXPIRED` is the terminal discard for builder-side
windows that can never become valid again, such as an expired bundle validity
window or an expired position predicate. `BUILDER_ACCEPTED` and
`BUILDER_INCLUDED` are unchanged; correlate a deferral with a later
accept/include by `tx_hash` within the same `payload_id`/block window. A
parking-capacity miss stays `BUILDER_REJECTED` with
`validity_predicate_not_satisfied`.
`BUILDER_INCLUDED` is emitted when the builder finalizes the payload it can
serve via `engine_getPayload` and includes
`data.inclusion_signal = "builder_finalized_payload"`. The payload loop emits
nonce and validation skips as `BUILDER_REJECTED` rather than inventing
replacement relationships. `BUILDER_PAYLOAD_FINALIZED` is emitted once for each
built payload and links `payload_id` to the builder's block hash and number even
when the payload contains no user transactions. It includes `data.parent_hash`,
`data.transaction_count`, `data.gas_used`, `data.gas_limit`, and
`data.timestamp`.

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

## Mempool / Builder Examples

Validity admission. This is the only event that carries `data.validity_predicates`.
Join later park, expiry, accept, and include events by `tx_hash`:

```json
{
  "schema_version": "transaction-event/v1",
  "event_id": "0x4c6d...",
  "event_time": "2026-06-02T00:00:00.000000000Z",
  "producer": "base-reth-node",
  "event_type": "TXPOOL_SEND_RAW_TRANSACTION_VALIDITY",
  "network": "base-mainnet",
  "tx_hash": "0x3333333333333333333333333333333333333333333333333333333333333333",
  "block_hash": null,
  "block_number": null,
  "payload_id": null,
  "request_id": null,
  "data": {
    "rpc_method": "base_sendRawTransactionValidity",
    "validity_predicates": [
      {
        "type": "storage",
        "params": {
          "address": "0xabababababababababababababababababababab",
          "slot": "0x1",
          "mask": "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
          "op": "=",
          "value": "0x1"
        }
      }
    ]
  }
}
```

Parked (recoverable predicate, held for a later position or block). Does
not repeat the predicate list:

```json
{
  "schema_version": "transaction-event/v1",
  "event_id": "0x5d7e...",
  "event_time": "2026-06-02T00:00:00.200000000Z",
  "producer": "base-builder",
  "event_type": "BUILDER_DEFERRED",
  "network": "base-mainnet",
  "tx_hash": "0x3333333333333333333333333333333333333333333333333333333333333333",
  "block_hash": null,
  "block_number": 123,
  "payload_id": "0x0102030405060708",
  "request_id": null,
  "data": {
    "builder_mode": "native",
    "ordering_position": 4,
    "defer_reason": "validity_predicate_not_satisfied",
    "defer_detail": "a validity predicate is not satisfied by the current build state"
  }
}
```

Terminal builder-side expiry. A parking-capacity miss stays `BUILDER_REJECTED`
with `validity_predicate_not_satisfied` instead:

```json
{
  "schema_version": "transaction-event/v1",
  "event_id": "0x6e8f...",
  "event_time": "2026-06-02T00:00:00.400000000Z",
  "producer": "base-builder",
  "event_type": "BUILDER_EXPIRED",
  "network": "base-mainnet",
  "tx_hash": "0x4444444444444444444444444444444444444444444444444444444444444444",
  "block_hash": null,
  "block_number": 123,
  "payload_id": "0x0102030405060708",
  "request_id": null,
  "data": {
    "builder_mode": "native",
    "ordering_position": 1,
    "expire_reason": "validity_predicate_expired",
    "expire_detail": "a validity predicate can no longer be satisfied at or after the current build position"
  }
}
```
