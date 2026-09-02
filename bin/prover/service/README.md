# `base-prover-service`

Standalone JSON-RPC binary for the Base prover service.

Runs the prover-service requester and worker APIs, backed by Postgres. It queues
proof requests, leases work to external workers, tracks heartbeats, and stores
submitted results.

This binary does not run proving backends in-process. ZK and TEE proof
generation runs in separate worker processes that claim jobs through the worker
API.

## Security Model

The unauthenticated requester and worker APIs are internal service endpoints.
Restrict them to trusted components with private-network controls; never expose
them publicly. Wildcard binds support container networking: the in-container
listener must accept forwarded traffic, so the deployment — not
`RPC_LISTEN_ADDR` — decides who can reach it.

The requester API is the more sensitive of the two. It accepts a
caller-controlled `zk_backend`, including the paid `network` variant, so a
reachable port lets any caller spend the deployment's requester funds, with no
cumulative cap. It also exposes `deleteProofRequest`, `deleteProofsByTeeSigner`,
and `listProofs`, so a reachable port allows deleting proofs already paid for.

The standalone Compose stack therefore publishes the requester port on
`127.0.0.1` only (see `etc/docker/docker-compose.prover.yml`). Reaching it from
another host requires putting your own authenticated proxy in front of the
loopback port, not republishing the port on a public interface. See
[docs/guides/STANDALONE_PROVING.md](../../../docs/guides/STANDALONE_PROVING.md).
