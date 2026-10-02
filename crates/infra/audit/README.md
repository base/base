# `audit-archiver-lib`

Audit library for Postgres-backed transaction observability events.

## Overview

Accepts Vector's `transaction-event/v1` batches over HTTP, persists them
idempotently in Postgres, and provides JSON-RPC queries by transaction, block,
bundle, and rejection. Postgres schema readiness is checked before the
`audit-archiver` binary starts serving; see the binary README for the
S3 removal and rollout prerequisites.

## Postgres schema

`migrations/` holds the transaction event schema. `schema.sql` is a committed
`pg_dump` of the schema those migrations produce, without dated day partitions.
The Postgres integration tests fail if a fresh or upgraded database differs
from it. Applied migration SQL/checksums are immutable; add a new migration for
a schema change. For a newly added migration, regenerate and review the diff:

```bash
UPDATE_SCHEMA_SNAPSHOT=1 cargo test -p audit-archiver-lib \
  --test postgres_transaction_events postgres_schema_matches_committed_snapshot
```

`legacy_migrations/` holds the pre-partition migrations 001-004. They are never
applied. The migrator recognizes their recorded rows by version and checksum,
then drops the old table and resets that history in the same transaction that
applies the partitioned baseline.

## Migration lifecycle

`PgTransactionEventSink::migrate` remains schema-only for compatibility and
schema fixtures. `AuditMigration::run` implements ordinary `migrate up`: schema,
registered online work outside transactions, then catalog validation. Schema
and reconciliation share one dedicated session advisory lock. `RequiredAuditWork`
is the ordered upstream registry; new required online operations belong there
with their prerequisite, resumable reconciliation, validation, and behavioral
tests. Do not change applied migration SQL to register runtime work.

`ManagedMigration` hosts that lifecycle and terminal results on one probe/status/
metrics listener, with exact-owned Postgres cancellation and same-pod state.
See the binary README for deployment, restart, readiness, and stop contracts.

The `native_migration` acceptance tests refuse foreign data directories and
require an explicitly owned Postgres 17 cluster under this checkout's `.tmp/`.
Set `TIPS_AUDIT_TEST_POSTGRES_URL` and `TIPS_AUDIT_TEST_CLUSTER_PATH` to that
cluster. Run from the repository root with the actual local binary:

```bash
cargo build -p audit-archiver
export TIPS_AUDIT_TEST_BINARY="$PWD/target/debug/audit-archiver"
cargo test -p audit-archiver-lib --test native_migration -- --ignored --test-threads=1
```

These tests cover real blocked concurrent builds and cancellation; unit tests
or compile success alone are not database lifecycle acceptance.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
