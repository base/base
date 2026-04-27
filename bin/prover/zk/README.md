# `base-prover-zk`

ZK prover service binary.

Runs the gRPC ZK prover server. Reads proof requests from a database outbox, dispatches them to a cluster backend, and stores artifacts in Redis, S3, or GCS.
