# Base Prover Service

`base-prover-service` defines the gRPC contract used to submit proof requests,
poll proof status, and coordinate worker-owned proof jobs. It also provides the
service implementation, proving backends, worker pool, and RPC proxy.
