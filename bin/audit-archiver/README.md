# `audit-archiver`

Accepts transaction observability events over HTTP, stores them in Postgres,
and serves Postgres-backed transaction event queries over JSON-RPC.
`TIPS_AUDIT_POSTGRES_URL` is required to serve; startup fails if the Postgres
schema is not ready, which includes migration `003_transaction_events_v2.sql`;
run `migrate up` before rolling out this release. This S3 removal needs no new
database migration.
If still on the pre-partition schema, `migrate up` resets the table and deletes
its old rows: export them or agree on retention before running that migration.

## Security Model

The unauthenticated RPC and ingest APIs are internal endpoints. Restrict them to
trusted producers with private-network controls; never expose them publicly. The
wildcard bind supports container networking.

`audit-archiver` accepts transaction observability event batches over HTTP and
stores them in Postgres.
The HTTP ingest endpoint is intended for Vector and accepts newline-delimited
JSON, with one `transaction-event/v1` object per line:

```bash
printf '{"schema_version":"transaction-event/v1","event_id":"example-builder-accepted-1","event_time":"%s","producer":"base-builder","event_type":"BUILDER_ACCEPTED","network":"base-mainnet","tx_hash":"0x1111111111111111111111111111111111111111111111111111111111111111","block_hash":null,"block_number":null,"payload_id":null,"request_id":null,"data":{"position":1}}\n' \
  "$(date -u +%Y-%m-%dT%H:%M:%SZ)" |
  curl -sS -X POST "http://127.0.0.1:9100/v1/transaction-events/batch" \
    -H "content-type: application/x-ndjson" \
    --data-binary @-
```

The endpoint is intended for Vector HTTP output from the dedicated transaction
event journal. It is not a stdout/stderr log ingestion endpoint.

The legacy S3 archive, bundle-event RPC ingest, and rejected-transaction
S3 RPC ingest are removed. The old RPC methods return "method not found";
they never acknowledge events that would be discarded. Existing S3 history is
not migrated; decide whether to export or retain it before deleting buckets.

The old S3 rejection feed only covered enforced per-transaction execution-time
rejections. `BUILDER_REJECTED` records that decision, its reason, predicted time,
and limit in the journal, but not the full S3 `MeterBundleResponse`. Bundle
lifecycle events have no one-to-one transaction-journal equivalent. The old
builder forwarder and bundle connector log and drop failed RPC batches; verify
no legacy bundle sender remains and journal rejections reach Postgres before
deploying this binary. Chart settings alone are not proof.

Roll out only after the old ingress bundle sender is gone and the builder's
transaction event journal is enabled. On each network, compare
`BUILDER_REJECTED` emissions with events queryable from Postgres and monitor
`transaction_events_persisted`, `transaction_events_rejected`, and Vector
discard/retry metrics during the soak. `TIPS_AUDIT_NOOP_ARCHIVE` previously
covered bundle workers only: rejected-transaction RPC calls could still write
to S3. Confirm S3 writes have stopped before removing S3 IAM or buckets.

To verify the local devnet path end-to-end:

```bash
just devnet tx-observability
just devnet tx-observability-smoke
```

## Transaction event retention

Ingest writes `transaction_events_v2`, which is partitioned by retention class
(`hot`, `warm`, `cold`), then by UTC day of `event_time`. Retention drops whole
day partitions instead of deleting rows, so expiry creates no dead tuples,
index bloat, or vacuum work.

The primary key is `(event_hour, retention_class, event_id)`, where
`event_hour` is the UTC hour of `event_time`. Leading with the hour keeps
inserts in the current hour's key range instead of spreading them across the
whole day's index. A retried or re-emitted `event_id` dedupes only within the
same UTC hour of `event_time`; a re-emission in another hour stores a second
row. `event_id`, `tx_hash`, and `block_hash` use `COLLATE "C"`. Hashes are
stored only as `0x` followed by 64 lowercase hex digits, which a `CHECK`
enforces.

### Legacy tree

Migrations 001 and 002 created an earlier `transaction_events` tree.
Migration 003 created `transaction_events_v2` beside it without copying rows.
The service no longer reads, writes, or maintains the legacy tree, and a later
migration drops it.

### Direct reads and warehouse extraction

Ad hoc queries should filter hashes by their lowercase `0x` form, since no
other form is stored. Migration 003 grants `SELECT` on the
`transaction_events_v2` parent to the `datapilot` extraction role when that
role exists. Leaf partitions get no grants because reads through the parent
need none. Migration 003 builds BRIN indexes on `ingested_at` and `event_seq`,
and each day partition gets both when it attaches.

`event_seq` is a `BIGINT` identity column that numbers v2 rows in insertion
order. Inserts through the parent take it from one sequence shared by every
partition, and the runtime role needs no grant on that sequence. Values are
unique, but each connection reserves 100 at a time, so they can be out of
order across connections and have gaps. It exists for parallel extraction:
DataPilot splits each incremental window into `event_seq` ranges, and because
`event_seq` grows with physical row order, the `event_seq` BRIN index lets
each range read only its own blocks. A hash such as `event_id` would make
every range read the whole window. The DataPilot table entry for v2 uses:

```json
{
  "incremental_load_column": "ingested_at",
  "split_by_int_column": "event_seq",
  "overwrite_by_columns": ["event_id", "event_hour", "retention_class"]
}
```

with a small `NUM_SLICES` (DataPilot recommends 2-16 for incremental loads).

Classes: hot (high-volume proxy and builder-decision events), warm (ingress,
simulation success, txpool-forward), and cold (failures, drops, inclusion,
flashblocks). An event's class comes from its `event_type` at ingest and is
stored in `retention_class`.

### Partition maintenance

When `TIPS_AUDIT_POSTGRES_URL` is set, a background worker maintains
partitions. It uses a dedicated one-connection Postgres pool and
`pg_try_advisory_lock`, so only one replica changes partitions at a time and
lock losers do not occupy ingest connections. The first pass runs at startup;
later passes wait the retention interval, skipping missed ticks.

Each pass, for each class:

- creates missing day partitions from the start of the retention window
  through three days after today, so ingest keeps working for three days if
  maintenance stops
- drops day partitions that are entirely older than the
  retention window plus a one-hour grace period

The runtime role does not own the tables, so partition DDL goes through
`SECURITY DEFINER` functions created by `003_transaction_events_v2.sql`,
executable only by `audit_archiver`. Create uses `CREATE TABLE` + `ATTACH PARTITION`, which only
takes a `SHARE UPDATE EXCLUSIVE` lock on the class partition. Drop detaches
first (a brief `ACCESS EXCLUSIVE` lock on the class partition, which queues
inserts for that class) and then drops the detached table in a separate
transaction. Each statement runs under
`TIPS_AUDIT_TRANSACTION_EVENT_PARTITION_LOCK_TIMEOUT_MS`; a statement that
times out is skipped and retried on the next pass.

### Ingest admission

Ingest rejects events whose `event_time` is older than their class's retention
window or more than one hour in the future. Those events would have no
partition, and one bad timestamp would otherwise fail the whole insert batch.
Rejected events count toward `transaction_events_outside_retention_window`.

Watch `transaction_event_partition_horizon_seconds`, which every replica
refreshes from the catalog each pass, net of the one-hour future skew (alert
well before it reaches zero), `transaction_event_partitions_created`,
`transaction_event_partitions_dropped`,
`transaction_event_partition_lock_timeouts`, and
`transaction_event_retention_failures`.

### Environment

- `TIPS_AUDIT_TRANSACTION_EVENT_RETENTION_INTERVAL_SECS` (default `3600`, at most `86400`): seconds between partition maintenance passes
- `TIPS_AUDIT_TRANSACTION_EVENT_HOT_RETENTION_DAYS` (default `3`)
- `TIPS_AUDIT_TRANSACTION_EVENT_WARM_RETENTION_DAYS` (default `7`)
- `TIPS_AUDIT_TRANSACTION_EVENT_COLD_RETENTION_DAYS` (default `30`, at most `90`)
- `TIPS_AUDIT_TRANSACTION_EVENT_PARTITION_LOCK_TIMEOUT_MS` (default `5000`): Postgres `lock_timeout` per partition create, detach, or drop
