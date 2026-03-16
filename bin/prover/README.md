# `base-prover`

Prover binary supporting TEE and ZK proving backends.

## `nitro` — TEE (AWS Nitro Enclaves)

- **`nitro server`** — Runs the JSON-RPC server on the EC2 host, forwarding proving requests to the enclave over vsock.
- **`nitro enclave`** — Runs the proving process inside the Nitro Enclave, listening on vsock.
- **`nitro local`** *(feature-gated)* — Runs server and enclave in a single process for local development.

## `zk` — ZK prover service

Runs the gRPC ZK prover server. Reads proof requests from a database outbox, dispatches them to a cluster backend, and stores artifacts in Redis, S3, or GCS.
