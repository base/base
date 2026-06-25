# `base-prover-zk`

ZK prover service binary.

Runs the gRPC ZK prover server. Reads proof requests from a database outbox, dispatches them to a cluster backend, and stores artifacts in Redis, S3, or GCS.

Set `SP1_PROVER=dry-run` to generate a real witness and execute the SP1 range program locally without producing a proof. Dry-run results are returned from `GetProofResponse.execution_stats`.
