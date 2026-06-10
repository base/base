# base-proof-zk-host

Host-side ZK proving primitives for prover-service workers.

The `ZkProver` trait captures the backend proving step independently from worker
job discovery and submission. Concrete backends implement this trait so worker
hosts can submit, poll, and download proofs through a common interface.
