# base-proof-zk-host

Host-side ZK proving worker for the prover service.

This crate adapts the shared worker machinery for ZK proving jobs. It provides
[`ZkHost`] and [`ProofGenerator`], which claim ZK jobs, drive a [`ZkProver`]
backend to completion, and hand proof results to `base_proof_worker::ProofSubmitter`.

Concrete SP1 proving backends are provided by `base-proof-zk-backend`.

[`ZkHost`]: crate::ZkHost
[`ProofGenerator`]: crate::ProofGenerator
[`ZkProver`]: crate::ZkProver
