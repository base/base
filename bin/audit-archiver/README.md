# `audit-archiver`

Reads audit log events via RPC and archives them to S3.

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
