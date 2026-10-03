# `audit-archiver`

Accepts transaction observability events over HTTP, stores them in Postgres,
and serves Postgres-backed transaction event queries over JSON-RPC.
`TIPS_AUDIT_POSTGRES_URL` is required to serve; startup fails if the Postgres
schema is not ready. This S3 removal needs no new database migration.
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

`transaction_events` is partitioned by retention class (`hot`, `warm`,
`cold`), then by UTC day of `event_time`. Retention drops whole day partitions
instead of deleting rows, so expiry creates no dead tuples, index bloat, or
vacuum work, and each day's indexes stay small enough to cache.

Classes: hot (high-volume proxy and builder-decision events), warm (ingress,
simulation success, txpool-forward), and cold (failures, drops, inclusion,
flashblocks). An event's class comes from its `event_type` at ingest and is
stored in `retention_class`.

### Partition maintenance

When `TIPS_AUDIT_POSTGRES_URL` is set, a background worker maintains
partitions. It uses a dedicated one-connection Postgres pool and
`pg_try_advisory_lock`, so only one replica changes partitions at a time and
lock losers do not occupy ingest connections. The first pass runs at startup;
later passes wait the retention interval, skipping missed ticks. A pass that
fails, or whose DDL hits a lock timeout, retries after 60 seconds instead. A
pod that starts before its network's migration therefore picks up the new
schema within a minute.

Each pass, for each class:

- creates missing day partitions from the start of the retention window
  through three days after today, so ingest keeps working for three days if
  maintenance stops
- drops day partitions that are entirely older than the retention window plus
  a one-hour grace period

The runtime role does not own the table, so partition DDL goes through
`SECURITY DEFINER` functions created by the baseline migration
(`001_transaction_events_partitioned.sql`) and executable only by
`audit_archiver`. Create uses `CREATE TABLE` + `ATTACH PARTITION`, which only
takes a `SHARE UPDATE EXCLUSIVE` lock on the class partition. Drop detaches
first (a brief `ACCESS EXCLUSIVE` lock on the class partition, which queues
inserts for that class) and then drops the detached table in a separate
transaction. Each statement runs under
`TIPS_AUDIT_TRANSACTION_EVENT_PARTITION_LOCK_TIMEOUT_MS`; a statement that
times out is skipped and retried by that 60-second retry.

### Shutdown

`audit-archiver` handles SIGTERM and SIGINT with a graceful HTTP shutdown: it
stops accepting connections, finishes in-flight requests, and closes idle
keep-alive connections. Without this, the binary runs as PID 1 in the
container, ignores SIGTERM, and keeps answering kept-alive producer
connections until SIGKILL, which during a blue-green rollout sends a few
batches to the old release.

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

## Incremental warehouse extraction index

Migration `002_transaction_events_ingested_at_index.sql` registers a BRIN index
on the partitioned `transaction_events` table and its hot/warm/cold parents.
It uses `ON ONLY`: `migrate up` creates metadata quickly but does **not** build
indexes on existing day partitions. The root index remains invalid until the
day indexes are built and attached. Future day partitions automatically get
their index on attach, even while the parent index is being completed.

After deploying the migrator, arrange a separate, monitored one-off run using
the `audit_archiver_migration` database credential:

```bash
# TIPS_AUDIT_POSTGRES_URL must point at the target network database.
audit-archiver index
```

The `index` command builds one BRIN index at a time with `CREATE INDEX
CONCURRENTLY` and attaches it to the class index. It is intentionally separate
from the chart's `migrate up` init container: production has many populated
day partitions, and building them can take hours. Re-running the command is
safe; it skips attached indexes and drops/rebuilds invalid indexes left by a
canceled concurrent build. Monitor Postgres storage, read I/O, and ingest
latency during the build. Do not run two index jobs against the same database;
the command also holds the migration lock to serialize them.

Check completion in each network database:

```sql
SELECT c.relname, i.indisvalid
FROM pg_index i
JOIN pg_class c ON c.oid = i.indexrelid
WHERE c.relname IN (
    'transaction_events_ingested_at_idx',
    'transaction_events_hot_ingested_at_idx',
    'transaction_events_warm_ingested_at_idx',
    'transaction_events_cold_ingested_at_idx'
);
```

All four should report `indisvalid = true`. The `ingested_at` BRIN index
serves DataPilot's timestamp cutoff; it does not by itself index an epoch
expression used for parallel slicing. Evaluate that expression's query plan
separately before enabling `NUM_SLICES` on the production primary.
