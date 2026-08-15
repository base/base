# base-proof-zk-backend

ZK proving backend implementations for prover-service workers.

Concrete backends implement the `ZkProver` abstraction from `base-proof-zk-host`
so worker hosts can submit, poll, and download proofs through a common
interface.

## Backends

- `MockZkProver`: instant placeholder proofs for exercising the full worker flow.
- `DryRunZkProver`: local SP1 execution statistics with an empty proof payload.
- `ClusterZkProver`: SP1 cluster range-proof backend for compressed proofs.
- `NetworkZkProver`: SP1 prover-network range-proof backend for compressed proofs.
