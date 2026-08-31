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
`transaction_events` rows. Retention uses a dedicated Postgres pool so lock
losers do not occupy ingest connections.

- `TIPS_AUDIT_TRANSACTION_EVENT_RETENTION_INTERVAL_SECS` (default `3600`): seconds between retention passes, and the maximum age of one expire scan cycle
- `TIPS_AUDIT_TRANSACTION_EVENT_HOT_RETENTION_DAYS` (default `3`)
- `TIPS_AUDIT_TRANSACTION_EVENT_WARM_RETENTION_DAYS` (default `7`)
- `TIPS_AUDIT_TRANSACTION_EVENT_COLD_RETENTION_DAYS` (default `30`)
- `TIPS_AUDIT_TRANSACTION_EVENT_RETENTION_BATCH_SIZE` (default `10000`): rows deleted per statement
- `TIPS_AUDIT_TRANSACTION_EVENT_RETENTION_MAX_BATCHES` (default `1000`): delete statements per locked pass
- `TIPS_AUDIT_TRANSACTION_EVENT_RETENTION_STATEMENT_TIMEOUT_MS` (default `30000`): statement timeout per expire DELETE
- `TIPS_AUDIT_POSTGRES_RETENTION_MAX_CONNECTIONS` (default `1`): connections in the retention pool

