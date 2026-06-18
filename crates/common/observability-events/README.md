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
  queueing, dropped-event metrics, write-error metrics, queue depth, and bytes
  written metrics.

## Contract Notes

Required envelope fields are `schema_version`, `event_id`, `event_time`,
`producer`, and `event_type`. Producers should include at least one join key
whenever available: `tx_hash`, `block_hash`/`block_number`, or `payload_id`.

Producer-specific fields belong in `data`. Do not put raw transaction bytes,
calldata, full request bodies, API keys, secrets, or raw forwarding headers in
transaction events.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
