# TIPS S3/Postgres Parity Checklist

Use this checklist before switching `TIPS_UI_QUERY_BACKEND=audit` in an
environment. Keep the S3-backed route enabled until the same samples pass
against audit-archiver/Postgres.

## Sample Inputs

- One recent block hash with at least one non-system transaction.
- The same block by number.
- One transaction hash from that block with a `SIMULATION_SUCCEEDED` event.
- One bundle hash or bundle id from that transaction's event data.
- One rejected transaction from the last 31 days, if present.

## Route Comparisons

- `GET /api/block/<block-hash>` returns the same block hash, number, gas limits,
  transaction order, and transaction hashes.
- `GET /api/block/<block-number>` resolves to the same canonical block as the
  hash route.
- For each sampled non-system transaction, the S3 and Postgres responses agree
  on whether simulation data exists.
- When simulation data exists, compare bundle hash/id, state block number, total
  gas used, total execution time, and per-transaction gas/time summaries.
- `GET /api/txn/<tx-hash>` returns the same transaction hash and at least one
  bundle id/hash join key.
- `GET /api/bundle/<bundle-id-or-hash>` returns events in event-time order and
  includes the accepted simulation event for the sampled transaction.
- `GET /api/rejected` returns the same sampled rejected transaction hash, block
  number, rejection reason, timestamp, and metering summary when a sample exists.

## Operational Checks

- `audit-archiver` has `TIPS_AUDIT_POSTGRES_URL` set and serves both JSON-RPC
  and transaction-event HTTP ingest on the audit service port. Vector posts
  NDJSON to `/v1/transaction-events/batch` on that same port.
- `ingress-rpc` has `TIPS_INGRESS_TRANSACTION_EVENTS_ENABLED=true` only in the
  environment being tested, with the JSONL file path matching the Vector
  sidecar mount.
- The UI has `TIPS_UI_QUERY_BACKEND=audit` and `TIPS_UI_AUDIT_RPC_URL` pointed at
  the audit service.
- S3 objects are still being written during the comparison window.
- No Kafka, S3, or existing RPC persistence removal is included in the rollout.
