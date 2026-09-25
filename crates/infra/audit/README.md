# `audit-archiver-lib`

Audit library for tracking and archiving bundle events.

## Overview

Provides event publishing, storage, and retrieval for bundle lifecycle events. `AuditConnector`
wires an event receiver to a publisher, `RpcBundleEventPublisher` publishes events over RPC,
and `S3EventReaderWriter` archives events to S3 for long-term retention. Also exposes
`LoggingBundleEventPublisher` for local development.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
audit-archiver-lib = { workspace = true }
```

```rust,ignore
use audit_archiver_lib::{AuditConnector, RpcBundleEventPublisher};

let publisher = RpcBundleEventPublisher::new(rpc_url, timeout)?;
AuditConnector::connect_batched(event_rx, publisher, batch_size, batch_wait);
```

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
