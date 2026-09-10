# Base Prover Service

`base-prover-service` defines the JSON-RPC contract used to submit proof
requests, poll proof status, and coordinate worker-owned proof jobs. It also
provides the service implementation and queue-maintenance status polling.

Requester-submitted TEE work is queued in `proof_requests` and claimed through
the worker API. Nitro/TEE hosts own enclave integration.

SP1 proving has been removed. New compressed and SNARK/PLONK requests fail with
an explicit unsupported error before being queued. Legacy ZK records and protocol
types remain readable. See [CAVEATS.md](../../../CAVEATS.md).

Enable `rpc-server` to generate the server trait. Use
`base-proof-service-client` for requester and worker JSON-RPC clients.
