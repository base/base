# `base-prover-service`

Standalone JSON-RPC binary for the Base prover service.

Runs the prover-service requester and worker APIs, backed by Postgres. It queues
proof requests, leases work to external workers, tracks heartbeats, and stores
submitted results.

This binary does not run proving backends in-process. ZK and TEE proof
generation runs in separate worker processes that claim jobs through the worker
API.
