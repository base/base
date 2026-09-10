# Base Prover Service

`base-proof-service-server` implements proof-request coordination, worker job
queues, status polling, and PostgreSQL storage. Its migrations preserve the
existing database schema; see [DATABASE.md](DATABASE.md) for storage details.

Requester-submitted TEE work is queued in `proof_requests` and claimed through
the worker API. Nitro/TEE hosts own enclave integration.

SP1 proving has been removed. New compressed and SNARK/PLONK requests fail with
an explicit unsupported error before being queued. Legacy ZK records and protocol
types remain readable. See [CAVEATS.md](../../../../CAVEATS.md).

The wire contract lives in `base-proof-service-protocol`. Use
`base-proof-service-client` for requester and worker JSON-RPC clients.
