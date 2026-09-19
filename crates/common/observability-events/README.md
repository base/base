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
- **`TransactionEventWriter`**: Non-blocking JSONL append writer with bounded
  queueing, aggregate dropped-event metrics, write-error metrics, and bytes
  written metrics. After event-producing tasks stop, `shutdown(timeout)` drains
  already queued events, flushes the active file, and closes it. The wait is
  bounded so a blocked worker cannot hang process exit. Forced termination
  (SIGKILL, OOM, abort, or shutdown timeout) can still lose queued events and
  leave a partial last line.
- **`TransactionEventBuilder`** and **`transaction_event!`**: Helpers for
  producer call sites that use the process-global transaction event writer while
  filling common envelope fields such as `event_time`, `network`, join keys,
  deterministic event IDs, and write-failure logging.

## Contract Notes

Required envelope fields are `schema_version`, `event_id`, `event_time`,
`producer`, and `event_type`. Producers should include at least one join key
whenever available: `tx_hash`, `block_hash`/`block_number`, or `payload_id`.

Producer-specific fields belong in `data`. Do not put raw transaction bytes,
calldata, full request bodies, API keys, secrets, private keys, tokens, or raw
forwarding headers in transaction events. Rust validation rejects a small exact
denylist; collector pipelines should enforce broader key-pattern filtering before
ingest.

The writer is best-effort on the emit path after initialization. Runtime write
or flush failures are reported through metrics and logs, but they do not block
transaction-serving paths. Collectors must tolerate and skip malformed JSONL
lines because storage failures such as disk-full conditions can leave a
partial line in the file.

Graceful process shutdown is different: producers stop first, then
`shutdown(timeout)` atomically rejects new events (drop reason `shutdown`),
drains events already in the queue, flushes the active file, and closes it.
That path returns write/flush errors and cannot wait longer than `timeout`.
It does not `fsync`. Forced termination does not run this drain.

Binaries should bind `GlobalTransactionEventWriter::drain_on_drop(timeout)` in
the scope that owns the process lifecycle rather than calling `shutdown` on each
return path. Declaring the guard before the producers makes drop order drain the
journal after they stop, and covers early `?` returns and unwinding. Call
`shutdown(timeout)` directly when the caller needs the error.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
