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
later passes wait the retention interval, skipping missed ticks.

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

## Incremental warehouse extraction index

Migration `002_transaction_events_ingested_at_index.sql` registers a BRIN index
on the partitioned `transaction_events` table and its hot/warm/cold parents.
Its `ON ONLY` SQL creates parent metadata without scanning populated days.
Ordinary `migrate up` now commits schema, runs the shared online reconciler
outside the schema transaction, validates the catalog, and only then succeeds.
Future day partitions inherit the index when attached. Applied SQL and sqlx
checksums remain immutable.

Run the full lifecycle with the migration-owner credential:

```bash
# TIPS_AUDIT_POSTGRES_URL must point at the target network database.
audit-archiver migrate up
```

The shared reconciler builds one BRIN index at a time with `CREATE INDEX
CONCURRENTLY`, then attaches it. Reruns skip valid attached indexes and repair
invalid unattached indexes left by interruption. The compatibility `index`
command still reconciles without applying schema. Both paths hold one sqlx
migration lock on a dedicated session, including final validation.

Historical scans can take hours. Do not deploy the full command in a database
init container or wait for index completion inside Codeflow V1/Sif's 14400-second
deployment deadline. Use native managed main-container execution below. Monitor
storage, read I/O, and ingest latency; deployment readiness is not completion.

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

## Native managed migration

```bash
export TIPS_AUDIT_MIGRATE_MANAGED=true
export TIPS_AUDIT_MIGRATION_RUN_ID=pod-uid-attempt-1
export TIPS_AUDIT_MIGRATION_STATE_PATH=/var/run/audit-migrator/state.json
export TIPS_AUDIT_METRICS_ENABLED=true
export TIPS_AUDIT_METRICS_PORT=9002
audit-archiver migrate up --managed
```

Only `migrate up` accepts managed execution. New arguments/environment:

- `--managed`: `TIPS_AUDIT_MIGRATE_MANAGED`, disabled by default.
- `--migration-run-id`: `TIPS_AUDIT_MIGRATION_RUN_ID`, required; 1–128 ASCII
  letters, digits, hyphens, underscores, dots, or colons. Use a nonsensitive
  pod UID plus reviewed attempt generation.
- `--migration-state-path`: `TIPS_AUDIT_MIGRATION_STATE_PATH`, required writable
  file on a same-pod volume. Its parent directory must already exist.
- `--migration-shutdown-timeout-secs`:
  `TIPS_AUDIT_MIGRATION_SHUTDOWN_TIMEOUT_SECS`, default `30`, range `1`–`30`.

Keep the existing migration-owner `TIPS_AUDIT_POSTGRES_URL`, metrics address,
interval, and logging configuration. Supply credentials through environment,
never command arguments. The main container must receive envmapper/all-env and
secret mounts before `exec` of the native binary. Retain secret-init only; remove
the database migration init step. Runtime API/ingest configuration is unchanged.
Use desired replicas `0`/`1`, `maxSurge: 0`, and `maxUnavailable: 1`. Disabled or
scale-zero means no schema or online work. Resume starts one resumable attempt.
A chart's old `operation=index` may map to full managed `migrate up`, but must
document that it now also applies schema; it must not remain an index-only job.

### One listener, availability, and completion

The metrics address/port (default `0.0.0.0:9002`) hosts all endpoints; recorder-only
installation prevents a competing exporter listener. Restrict this internal
listener to trusted probe/scrape clients. Disabling metrics does not disable
health or status.

- `GET /healthz`: `200` while the supervisor is responsive, including terminal
  operation failure. Do not use operation success as a liveness probe.
- `GET /readyz`: `503` until schema is committed/verified and the worker/control
  facility is available; `200` during indexing and succeeded idle. A terminal
  reconcile failure after schema may remain ready. Schema failure and stopping
  remain unready. Probes use cached state, not the busy DDL connection.
- `GET /status`: version-1 JSON with `mode=migrate_up`, `run_id`, `attempt`,
  `state=running|succeeded|failed|stopped`, `phase`, `schema_ready`,
  `worker_available`, `ready`, `complete`, `cancellation_confirmed`, timestamps,
  operation/partition, leaf progress counts, safe error code, and SQLSTATE.
  Phases are `starting`, `waiting_for_lock`, `schema`, `reconciling`,
  `validating`, `idle`, and `stopping`. `complete` is true only on success.
- `GET /metrics`: Prometheus scrape on the same listener.

Managed results stay idle and observable until signal; no tight in-process
retry and no exit merely because reconciliation failed. Alert on failed state
and missing/overdue completion; Codeflow deployment success does not certify
that indexes finished.

Metrics use `tips_audit_migration_` plus `state{state}`, `phase{phase}`,
`schema_ready`, `worker_available`, `complete`, `attempts_total`,
`failures_total{phase}`, `leaves_total`, `leaves_completed`, `leaves_built`,
`leaves_repaired`, `leaves_skipped`, `last_progress_timestamp_seconds`,
`duration_seconds`, and `cancellation_total{outcome}`. Cancellation outcomes are
`requested`, `confirmed`, and `unconfirmed`. Total/completed leaves are current-pass
gauges; built/repaired/skipped are process counters. Labels are bounded enums,
never run identities, partitions, backend PIDs, or error text. Status counts
describe the attempt; totals can change as partitions appear.

### Restart and deliberate retry

State writes are atomic, mode `0600`, and fsynced with their directory. Same-pod
container restarts restore succeeded/failed/stopped results without rerunning;
schema metadata is checked read-only before readiness. Interrupted running
attempts resume once only after their previous owned backend is verified gone.
A still-live prior backend fails closed instead of authorizing competing work.
A new reviewed run ID authorizes a new attempt; an already-valid database
performs verification without rebuilding indexes. Schema/work fingerprints
prevent restoring stale success for a changed requirement. Corrupt state fails
closed and requires inspected recovery, never silent execution.

An `emptyDir` survives container restarts, **not pod deletion or scale-zero**.
A new pod starts one new resumable attempt, including after an earlier failure.
Cross-pod terminal restoration requires separate durable storage; a run ID alone
does not provide it.

### Graceful stop and cancellation proof

SIGTERM/SIGINT immediately makes the worker unready and prevents new DDL dispatch.
An independent same-role control connection repeatedly cancels only the captured
PID/backend-start/database/role/generated application identity. It falls back to
terminating that exact backend and verifies disappearance before reporting
`stopped`. Neither socket/client drop nor a true `pg_cancel_backend` return is
proof that PostgreSQL stopped.

The absolute database stop budget is at most 30 seconds: one third for cooperative
cancel, one third for termination fallback, one third for disappearance/cleanup.
Use pod termination grace of at least 45 seconds; no preStop sleep is needed for
this non-traffic worker. Denied/unreachable/unverified cleanup reports
`cancellation_unconfirmed`, never a false stopped state. SIGKILL or network loss
cannot guarantee immediate cancellation. Interrupted concurrent builds may leave
invalid unattached indexes; the same reconciler repairs them on resume. Stop
never reverses schema migrations or deletes transaction data.

Connection, parser, and OS error text is omitted from managed diagnostics; logs
use static messages and safe structured fields. Statement and slow-query logging
is disabled on managed database connections. Use `TIPS_AUDIT_LOG_FORMAT=json`.
