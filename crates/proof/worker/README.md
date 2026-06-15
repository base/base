# base-proof-worker

Shared proof worker primitives for prover-service hosts.

This crate contains backend-neutral worker host contracts used by concrete proof
hosts. Higher-level polling, heartbeat, and submission loops can build on these
contracts without coupling to a specific proving backend.
