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
created the legacy `transaction_events` tree, which nothing uses; they stay
because sqlx verifies the checksums of applied migrations. `schema.sql` is a committed
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
