# `audit-archiver-lib`

Audit library for Postgres-backed transaction observability events.

## Overview

Accepts Vector's `transaction-event/v1` batches over HTTP, persists them
idempotently in Postgres, and provides JSON-RPC queries by transaction, block,
bundle, and rejection. Postgres schema readiness is checked before the
`audit-archiver` binary starts serving; see the binary README for the
S3 removal and rollout prerequisites.

## Postgres schema

`migrations/` holds the transaction event schema. `003` creates
`transaction_events_v2`, which the service reads and writes. `001` and `002`
created the legacy `transaction_events` tree and `004` drops it; they stay
because sqlx verifies the checksums of applied migrations. `005` adds the
function that summarizes v2's BRIN indexes. `006` and `007` tune bitmap-scan
prefetch and vacuum truncation. `008` adds the btree index on
`(ingested_at, event_seq)` that `DataPilot`'s extraction planning query scans
instead of the heap. `009` adds an `(event_hour, tx_hash)` index for lookups
by transaction hash, whose inserts stay within the current hour's slice of the
index, and `010` drops the `(tx_hash, event_time)` index it replaces. Day
partitions populated before `009` have no hash index, and lookups skip them.
`schema.sql` is a committed
`pg_dump` of the schema those migrations produce, without dated day partitions.
The Postgres integration tests fail if a fresh or upgraded database differs
from it. After changing a migration, regenerate the snapshot and review the
diff:

```bash
UPDATE_SCHEMA_SNAPSHOT=1 cargo test -p audit-archiver-lib \
  --test postgres_transaction_events postgres_schema_matches_committed_snapshot
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
