# base-proof-zk-backend

ZK proving backend implementations for prover-service workers.

Concrete backends implement the `ZkProver` abstraction from `base-proof-zk-host`
so worker hosts can submit, poll, and download proofs through a common
interface.

`OpSuccinctWitnessProvider` resolves each `ZkProofRequest` into a range witness
for all three backends. It owns the default sequence window and the L1 and
consensus RPC endpoints used to calculate the L1 head. A request can override
the sequence window or pin an exact L1 head. See [the ZK prover glossary](../GLOSSARY.md)
for the proof and witness terms.

The prover service and witness provider share `ZkProofRequest::validate_range`.
The block count must be a nonzero multiple of the intermediate block interval,
and the starting block plus the count must fit in `u64`. The service rejects
invalid ranges before enqueueing; the provider rejects them before any RPC call.

## Backends

- `DryRunZkProver`: local SP1 execution statistics with an empty proof payload.
- `ClusterZkProver`: SP1 cluster range-proof backend for compressed proofs.
- `NetworkZkProver`: SP1 prover-network range-proof backend for compressed proofs.

SP1 stdin, ELF/key setup, cluster clients, L2OO bindings, and stdin caches
live under `src/succinct`. Guest programs live in `crates/proof/zk/programs/succinct`.
The `OpenVM` range guest (local, not yet a proving backend) lives in
`crates/proof/zk/programs/openvm`.
