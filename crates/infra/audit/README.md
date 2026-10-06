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
because sqlx verifies the checksums of applied migrations. `schema.sql` is a committed
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

## Partitioned indexes

See [Add indexes to the partitioned transaction-event schema](../../../docs/transaction-events/partition-indexes.md)
for parent-only migrations, forward-only coverage versus historical backfill,
native reconciliation, and catalog validation. `migrate up` applies schema only;
the separate `audit-archiver index` command backfills the existing `ingested_at`
BRIN index. New index definitions do not register a generic backfill operation.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
