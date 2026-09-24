# base-proof-zk-backend

ZK proving backend implementations for prover-service workers.

Concrete backends implement the `ZkProver` abstraction from `base-proof-zk-host`
so worker hosts can submit, poll, and download proofs through a common
interface.

## Backends

- `DryRunZkProver`: local SP1 execution statistics with an empty proof payload.
- `ClusterZkProver`: SP1 cluster range-proof backend for compressed proofs.
- `NetworkZkProver`: SP1 prover-network range-proof backend for compressed proofs.

SP1 stdin, ELF/key setup, cluster clients, L2OO bindings, and stdin caches
live under `src/succinct`. Guest programs live in `crates/proof/zk/programs/succinct`.

## Checkpoint interval rollout

Range public values commit the requested checkpoint interval. Proofs generated
before this field was added cannot be decoded or aggregated by the new programs.
Drain or cancel existing proof sessions and regenerate cached proofs before
switching workers to the new programs. Rebuild and deploy the range and aggregation
ELFs together, with matching on-chain verification keys. The packed on-chain
journal format is unchanged.
