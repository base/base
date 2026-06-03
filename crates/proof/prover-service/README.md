# Base Prover Service

`base-prover-service` defines the JSON-RPC contract used to submit proof
requests, poll proof status, and coordinate worker-owned proof jobs. It also
provides the service implementation, proving backends, status polling, and RPC
proxy.

Requester-submitted work is queued in `proof_requests` and claimed through the
worker API for both ZK and TEE proof jobs. The protocol-native service uses
`proof_requests` as the single execution queue.

Enable `rpc-client` to generate a typed JSON-RPC client and `rpc-server` to
generate the server trait.
