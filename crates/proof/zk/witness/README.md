# base-proof-zk-witness

Host-side witness capture for ZK proving.

Fetches L1/L2 data over RPC, runs the preimage server, and collects
`DefaultWitnessData` (preimages + blobs) for a range. Callers must supply the
L1 head: this crate does not select one from the op-node safe-head database or from timestamps, and it does
not split block ranges. This crate is not a programs-workspace dependency:
guests consume witness bytes through zkVM stdin, not this host code.

SP1 stdin encoding lives in `base-proof-zk-backend`.
