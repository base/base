# `audit-archiver-lib`

Audit library for Postgres-backed transaction observability events.

## Overview

Accepts Vector's `transaction-event/v1` batches over HTTP, persists them
idempotently in Postgres, and provides JSON-RPC queries by transaction, block,
bundle, and rejection. Postgres schema readiness is checked before the
`audit-archiver` binary starts serving; see the binary README for the
S3 removal and rollout prerequisites.

## JSON-RPC read methods

All methods are in the `base` namespace and return `TransactionEventRecord`
values: the `transaction-event/v1` envelope plus `ingested_at`.

| Method | Parameters | Order |
|---|---|---|
| `base_getTransactionEventsByHash` | `tx_hash`, `limit?` | oldest first |
| `base_getTransactionEventsByBlockNumber` | `block_number`, `limit?` | oldest first |
| `base_getTransactionEventsByBlockHash` | `block_hash`, `limit?` | oldest first |
| `base_getTransactionEventsByBundle` | `bundle_key` (UUID or hash), `limit?` | oldest first |
| `base_getRejectedTransactionEvents` | `{fromBlock?, toBlock?, fromTime?, toTime?, limit?}` | newest first |

Oldest first means `event_time`, then `ingested_at`, then `event_id`. Newest
first means `event_time` then `event_id`, both descending. `limit` defaults to
500 and is clamped to 1..=2000.

These methods return a bare array, so a client cannot tell whether more events
matched than the limit allowed. Each method has a `V2` form with the same
parameters, events, and order, for example
`base_getTransactionEventsByHashV2`, that returns an object instead:

```json
{
  "events": [
    { "event_id": "...", "event_type": "BUILDER_ACCEPTED", "ingested_at": "2026-10-05T12:00:00.123Z" }
  ],
  "truncated": true
}
```

`truncated` is `true` when at least one more event matched beyond the
effective limit; the server fetches one extra row to find out. It is `false`
when every match is in `events`, including when exactly `limit` events match.
The `V2` methods have no cursor or total count. A truncated oldest-first list
holds the earliest events; a truncated rejected list holds the newest.

## Postgres schema

`migrations/` holds the transaction event schema. `schema.sql` is a committed
`pg_dump` of the schema those migrations produce, without dated day partitions.
The Postgres integration tests fail if a fresh or upgraded database differs
from it. After changing a migration, regenerate the snapshot and review the
diff:

```bash
UPDATE_SCHEMA_SNAPSHOT=1 cargo test -p audit-archiver-lib \
  --test postgres_transaction_events postgres_schema_matches_committed_snapshot
```

`legacy_migrations/` holds the pre-partition migrations 001-004. They are never
applied. The migrator recognizes their recorded rows by version and checksum,
then drops the old table and resets that history in the same transaction that
applies the partitioned baseline.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
