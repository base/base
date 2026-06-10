# base-proof-zk-backend

ZK proving backend implementations for prover-service workers.

Concrete backends implement the `ZkProver` abstraction from `base-proof-zk-host`
so worker hosts can submit, poll, and download proofs through a common
interface.
