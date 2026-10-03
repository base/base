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
- **`EventId`**: Random per-emission event IDs; redelivered copies keep theirs.
- **`TransactionEventWriter`**: Non-blocking JSONL append writer with bounded
  queueing, aggregate dropped-event metrics, write-error metrics, and bytes
  written metrics.
- **`TransactionEventBuilder`** and **`transaction_event!`**: Helpers for
  producer call sites that use the process-global transaction event writer while
  filling common envelope fields such as `event_id`, `event_time`, `network`,
  join keys, and write-failure logging.

## Contract Notes

Required envelope fields are `schema_version`, `event_id`, `event_time`,
`producer`, and `event_type`. Producers should include at least one join key
whenever available: `tx_hash`, `block_hash`/`block_number`, or `payload_id`.

### Event Identity

`event_id` is 32 random bytes, hex-encoded with a `0x` prefix, chosen when the
event is built. It identifies one emission, not the fact the event describes.
Two emissions about the same transaction always get different IDs, even when
every other field matches, so ingest never discards one observation as a
duplicate of another. To count or group facts (for example one row per
transaction, event type and payload), group by the join keys and `data` fields
at query time.

The ID is part of the serialized event. Collector retries, journal rotation
and replays resend the same line, so they keep its ID and ingest drops the
redelivered copy. Nothing regenerates an ID for an event that was already
built.

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
