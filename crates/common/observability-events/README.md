# `base-observability-events`

Shared transaction observability event envelopes and dedicated JSONL writer
utilities for Base services.

This crate defines the versioned `transaction-event/v1` event contract used by
transaction event producers. It is a business event journal for transaction
history and auditability, not an application logging path. Producers write these
events to dedicated JSONL files that collectors can tail independently from
stdout/stderr and the normal Kubernetes log pipeline.

## Overview

- **`TransactionEvent`**: Stable JSON envelope shared by Rust producers and
  mirrored by non-Rust producers.
- **`TransactionEventType`**: Versioned vocabulary for proxy, ingress, txpool,
  and builder transaction lifecycle events.
- **`EventIdBuilder`**: Helper for deterministic event IDs so downstream ingest
  can deduplicate retries.
- **`EventOccurrence`**: Per-emission identity (random process instance plus a
  process-wide sequence) for event types that record each occurrence.
- **`TransactionEventWriter`**: Non-blocking JSONL append writer with bounded
  queueing, aggregate dropped-event metrics, write-error metrics, and bytes
  written metrics.
- **`TransactionEventBuilder`** and **`transaction_event!`**: Helpers for
  producer call sites that use the process-global transaction event writer while
  filling common envelope fields such as `event_time`, `network`, join keys,
  deterministic event IDs, and write-failure logging.

## Contract Notes

Required envelope fields are `schema_version`, `event_id`, `event_time`,
`producer`, and `event_type`. Producers should include at least one join key
whenever available: `tx_hash`, `block_hash`/`block_number`, or `payload_id`.

### Event Identity

`event_id` is a SHA-256 hash of the producer, event type, join keys and any
`id` parts. Ingest keeps the first event per `event_id` and discards later
ones without comparing payloads, so two observations that hash the same inputs
collapse into one row.

Choose ID parts by what one event means:

- **Per occurrence.** RPC admissions, queue hand-offs, forward attempts and
  outcomes, and native-builder validity decisions record each time something
  happens. The same transaction can repeat across destinations, nodes,
  restarts, scans, batches and payload rebuilds, so these emitters add
  `occurrence: EventOccurrence::next()`.
- **Per process decision.** Flashblocks builder decisions are identified by
  payload ID, flashblock index and ordering position within one builder
  process. These emitters add `process_instance:
  EventOccurrence::process_instance()` so a repeated emission in one process
  still deduplicates, while another replica or a restarted builder that builds
  the same payload ID stays distinct.
- **Already distinct.** The txpool tracer hashes a per-transaction event index
  and the emission time in nanoseconds, so it needs neither.

The ID is computed once, when the event is built. Collector retries, journal
rotation and replays carry the serialized line unchanged, so they keep the same
ID and still deduplicate. Nothing recomputes an ID from its parts, and the
envelope fields and the `0x`-prefixed 64-hex-digit ID format do not change.

Producer-specific fields belong in `data`. Do not put raw transaction bytes,
calldata, full request bodies, API keys, secrets, private keys, tokens, or raw
forwarding headers in transaction events. Rust validation rejects a small exact
denylist; collector pipelines should enforce broader key-pattern filtering before
ingest.

The writer is best-effort after initialization. Runtime write or flush failures
are reported through metrics and logs, but they do not block transaction-serving
paths. Collectors must tolerate and skip malformed JSONL lines because storage
failures such as disk-full conditions can leave a partial line in the file.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
