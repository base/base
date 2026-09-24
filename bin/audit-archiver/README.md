# `audit-archiver`

Reads audit log events via RPC and archives them to S3.

## Security Model

The unauthenticated RPC and ingest APIs are internal endpoints. Restrict them to
trusted producers with private-network controls; never expose them publicly. The
wildcard bind supports container networking.

When `TIPS_AUDIT_POSTGRES_URL` is set, `audit-archiver` also accepts
transaction observability event batches over HTTP and stores them in Postgres.
The HTTP ingest endpoint is intended for Vector and accepts newline-delimited
JSON, with one `transaction-event/v1` object per line:

```bash
curl -sS -X POST "http://127.0.0.1:8080/v1/transaction-events/batch" \
  -H "content-type: application/x-ndjson" \
  --data-binary '{"schema_version":"transaction-event/v1","event_id":"example-builder-accepted-1","event_time":"2026-06-02T00:00:00Z","producer":"base-builder","event_type":"BUILDER_ACCEPTED","network":"base-mainnet","tx_hash":"0x1111111111111111111111111111111111111111111111111111111111111111","block_hash":null,"block_number":null,"payload_id":null,"request_id":null,"data":{"position":1}}
'
```

The endpoint is intended for Vector HTTP output from the dedicated transaction
event journal. It is not a stdout/stderr log ingestion endpoint.

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
`SECURITY DEFINER` functions created by migration 005 and executable only by
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

