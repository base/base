# `base-infra-audit`

Transaction audit storage and RPC services for Base.

`RejectedTransactionStore` persists rejected transaction records to S3.
`PgTransactionEventSink` ingests transaction observability events into Postgres and
provides history queries and retention management. `AuditArchiverRpc` exposes
rejected transaction ingestion and transaction history queries.

The audit service no longer accepts or publishes bundle lifecycle events.
Existing historical Postgres data and migrations remain intact.
