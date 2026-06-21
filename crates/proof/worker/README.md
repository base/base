# base-proof-worker

Shared proof worker loop for prover-service hosts.

This crate contains backend-neutral worker host contracts and shared polling,
heartbeat, job discovery, and proof submission loops for proof hosts that claim
jobs from prover-service, run backend-specific proof generation, and submit
proofs back through the worker API.
