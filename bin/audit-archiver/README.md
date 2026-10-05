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
row. `event_id` uses `COLLATE "C"`. `tx_hash` and `block_hash` are stored as
32-byte `BYTEA`, and read APIs return them as `0x` lowercase hex.

### Legacy tree and cutover

Migration `003_transaction_events_v2.sql` creates the v2 tree beside the
earlier `transaction_events` tree and does not copy rows. During a rolling
deploy, pods on the previous release keep inserting into `transaction_events`
while new pods insert into `transaction_events_v2`. Read APIs query both
trees, so rows from either release stay visible.

Maintenance creates no new legacy day partitions. It drops legacy days on the
same retention schedule as v2 days, so the legacy tree is empty once its last
seeded day ages out: up to the cold window (30 days by default) plus the
three seeded look-ahead days. A later migration can then drop it.

An event retried across the cutover can be stored once in each tree. The
bundle query collapses such pairs by `event_id`; the other queries return both
rows.

Rolling back to the previous release is safe for writes, but that release
reads only `transaction_events`, so rows written to the v2 tree are hidden
from its read APIs until the next roll forward.

`transaction_events_all` is a view over both trees in the legacy column
layout: `event_date` instead of `event_hour`, and hashes as `0x` hex text.
Consumers that read the table directly, such as incremental warehouse
extraction, must switch to the view when this migration ships, and their role
needs `SELECT` on it. Rows ingested before the switch are still selected by an
`ingested_at` watermark afterward, so a delayed switch delays extraction
without losing rows, as long as it happens before those rows age out. Filter
the view by `ingested_at` or `event_time`; hash and `event_id` predicates on
the view cannot use the v2 indexes.

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

- creates missing v2 day partitions from the start of the retention window
  through three days after today, so ingest keeps working for three days if
  maintenance stops
- drops day partitions in either tree that are entirely older than the
  retention window plus a one-hour grace period

The runtime role does not own the tables, so partition DDL goes through
`SECURITY DEFINER` functions created by
`001_transaction_events_partitioned.sql` (legacy tree) and
`003_transaction_events_v2.sql` (v2 tree), executable only by
`audit_archiver`. Create uses `CREATE TABLE` + `ATTACH PARTITION`, which only
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
refreshes from the v2 tree's catalog entries each pass, net of the one-hour future skew (alert
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

Migration `003_transaction_events_v2.sql` builds the v2 tree's
`ingested_at` BRIN index directly, because the tree is empty when it is
created; v2 day partitions get the index when they attach. The rest of this
section applies only to the legacy tree.

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
canceled concurrent build.

Attaching a day index needs an `ACCESS EXCLUSIVE` lock on it. Inserts only lock
the day they write to, but queries that do not filter by `event_date` (the
transaction, block, bundle, and rejection lookups) lock every day's indexes
for as long as they run. Each attach waits up to 30 seconds for its lock; new
queries that touch that day queue behind it meanwhile. If the wait times out,
the command moves on to the remaining days, then retries the deferred
attaches with exponential backoff (5s doubling to 60s between attempts) for
up to an hour before failing. It never cancels other sessions' queries.

The attach that completes a class index also validates the root index, which
needs an `ACCESS EXCLUSIVE` lock on `transaction_events` itself. That attach
locks the root table first so it cannot deadlock with reads, and waits at most
2 seconds for it, since every insert and read queues behind the wait. It is
retried like any other deferred attach.

Monitor Postgres storage, read I/O, and ingest
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
