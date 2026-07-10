# base-prover-zk-host

ZK prover-service worker host binary.

This binary is the CLI entry point for claiming ZK proof jobs from the prover service and running them with the requested ZK backend.

A complete RPC configuration always enables dry-run; cluster and network settings add those backends, while mock requires explicit opt-in.
`ZK_BACKEND` is no longer read; backend availability is inferred from these settings.
