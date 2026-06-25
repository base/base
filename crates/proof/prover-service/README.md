# Base Prover Service

`base-prover-service` defines the JSON-RPC contract used to submit proof
requests, poll proof status, and coordinate worker-owned proof jobs. It also
provides the service implementation and queue-maintenance status polling.

Requester-submitted work is queued in `proof_requests` and claimed through the
worker API for both ZK and TEE proof jobs. The protocol-native service uses
`proof_requests` as the single execution queue.

Concrete proving backends live behind worker hosts. ZK hosts own SP1
cluster/network/mock/dry-run integration, while TEE hosts own enclave
integration.

Enable `rpc-server` to generate the server trait. Use
`base-prover-service-client` for requester and worker JSON-RPC clients.
