# Base Prover Service

`base-prover-service` defines the JSON-RPC contract used to submit proof
requests, poll proof status, and coordinate worker-owned proof jobs. It also
provides the service implementation, proving backends, worker pool, and RPC
proxy.

Enable `rpc-client` to generate a typed JSON-RPC client and `rpc-server` to
generate the server trait.
