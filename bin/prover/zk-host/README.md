# base-prover-zk-host

ZK prover-service worker host binary.

This binary is the CLI entry point for claiming ZK proof jobs from the prover service and running them with the requested ZK backend.

Each deployment sets `PROVER_PROTOCOL_VERSION` (default `0`); the worker claims only jobs with
that exact version. Serving several versions needs one fleet per version — the multi-version
claim path is wired up for the Nitro host only.
