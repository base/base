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
just devnet ingress
just devnet tx-observability-smoke
```

## Transaction event retention

When `TIPS_AUDIT_POSTGRES_URL` is set, a background worker deletes expired
`transaction_events` rows. Expire is I/O-heavy and shares the instance with
HTTP ingest, so it is not a continuous `DELETE` loop.

The worker uses a dedicated one-connection Postgres pool and
`pg_try_advisory_lock`. Replicas that lose the lock return immediately and do
not occupy ingest connections. The first tick runs at startup. Later ticks wait
the retention interval. Missed ticks are skipped so a slow pass does not stack
catch-up work.

Rows expire by `ingested_at` in three classes, oldest first within a pass: hot
(high-volume proxy and builder-decision events), then warm (ingress, simulation
success, txpool-forward), then cold (failures, drops, inclusion, flashblocks).
The pass budget (`max_batches`) is shared across classes, so a large hot
backlog can defer warm and cold until a later pass.

### Two clocks

`TIPS_AUDIT_TRANSACTION_EVENT_RETENTION_STATEMENT_TIMEOUT_MS` (default `30000`)
is the Postgres `statement_timeout` on **each** expire `DELETE`. It is a stall
fuse, not a target runtime. A canceled statement rolls back so the connection
can be reused; any `ctid`s already found are discarded.

`TIPS_AUDIT_TRANSACTION_EVENT_RETENTION_INTERVAL_SECS` (default `3600`) is both
the worker tick spacing and the maximum wall-clock age of one in-pass scan
cycle. A cycle can run many 30s-capped statements. A single delete cannot run
for an hour. The hourly default matches the original pass budget (1000 batches
× 10000 rows) and keeps expire from deleting continuously.

### One delete batch

Each batch is one transaction: `SET LOCAL statement_timeout`, a keyset
`SELECT … FOR UPDATE SKIP LOCKED LIMIT n` on `(event_type, ingested_at,
event_id)`, then `DELETE` by `ctid`. `SKIP LOCKED` skips rows held by ingest;
it does not skip dead tuples or bound I/O. The resume cursor is the smallest
selected key so the next `WHERE key < cursor` walk does not rescan rows just
deleted.

The configured `LIMIT` (default 10000) is the size to use when the index walk
is dense. On `57014` (statement timeout), expire tries `LIMIT 1`. If that
succeeds, it bisects between the last full success and the timed-out size. If
`LIMIT 1` times out, that class stops for the rest of the pass; warm and cold
still run. Progress (`LIMIT` bounds and cursor) is in memory on the lock
holder for this pass.

When a cycle is older than the retention interval and at least one batch has
returned, expire restarts the cycle immediately on the same lock: new cutoff,
configured `LIMIT`, cleared cursor. The first batch of a cycle always runs,
even when the interval is 1s. A queued post-timeout `LIMIT 1` also runs before
restart. Cycle restart is not a replica handoff and does not wait for the
worker ticker.

Watch `transaction_events_expired`,
`transaction_events_expire_statement_timeouts`,
`transaction_event_retention_effective_batch_limit`, and
`transaction_event_retention_cycles_ended`. If a class times out the
configured `LIMIT` on every cycle, lower
`TIPS_AUDIT_TRANSACTION_EVENT_RETENTION_BATCH_SIZE` (or the statement timeout)
in that environment rather than shrinking the defaults.

### Environment

- `TIPS_AUDIT_TRANSACTION_EVENT_RETENTION_INTERVAL_SECS` (default `3600`): seconds between retention passes, and the maximum age of one expire scan cycle
- `TIPS_AUDIT_TRANSACTION_EVENT_HOT_RETENTION_DAYS` (default `3`)
- `TIPS_AUDIT_TRANSACTION_EVENT_WARM_RETENTION_DAYS` (default `7`)
- `TIPS_AUDIT_TRANSACTION_EVENT_COLD_RETENTION_DAYS` (default `30`)
- `TIPS_AUDIT_TRANSACTION_EVENT_RETENTION_BATCH_SIZE` (default `10000`): rows deleted per statement when the scan is not bisected
- `TIPS_AUDIT_TRANSACTION_EVENT_RETENTION_MAX_BATCHES` (default `1000`): delete statements per locked pass
- `TIPS_AUDIT_TRANSACTION_EVENT_RETENTION_STATEMENT_TIMEOUT_MS` (default `30000`): Postgres `statement_timeout` per expire `DELETE`

