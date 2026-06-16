# base-proof-zk-host

Host-side ZK proving worker for the prover service.

This crate adapts the shared worker machinery for ZK proving jobs. It provides
[`ProofGenerator`], which claims a ZK job, drives a [`ZkProver`] backend to
completion, and hands the proof result to `base_proof_worker::ProofSubmitter`.

The concrete SP1 backend is wired separately. [`UnimplementedZkProver`] is a
placeholder for early host wiring.

[`ProofGenerator`]: crate::ProofGenerator
[`ZkProver`]: crate::ZkProver
[`UnimplementedZkProver`]: crate::UnimplementedZkProver
