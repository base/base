# base-prover-service-worker-host

Shared prover-service worker host primitives.

This crate contains backend-neutral worker host contracts used by concrete proof
hosts. Higher-level polling, heartbeat, and submission loops can build on these
contracts without coupling to a specific proving backend.
