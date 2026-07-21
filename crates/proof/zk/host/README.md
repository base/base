# base-proof-zk-host

Host-side ZK proving worker for the prover service.

This crate adapts the shared worker machinery for ZK proving jobs. It provides
[`ZkHost`] and [`ProofGenerator`], which claim ZK jobs, drive the matching
[`ZkProver`] backend to completion, and hand proof results to
`base_proof_worker::ProofSubmitter`.

A host can run multiple backends at once. Each claimed job selects its backend
via `zk_backend` on the proof request; the host only claims backends it has
configured.

Concrete SP1 proving backends are provided by `base-proof-zk-backend`.

[`ZkHost`]: crate::ZkHost
[`ProofGenerator`]: crate::ProofGenerator
[`ZkProver`]: crate::ZkProver
