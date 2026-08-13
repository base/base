# base-proof-zk-witness

Host-side witness capture for ZK proving.

Fetches L1/L2 data over RPC, runs the preimage server, and collects
`DefaultWitnessData` (preimages + blobs) for a range. This crate is not a
programs-workspace dependency: guests consume witness bytes through zkVM
stdin, not this host code.

SP1 stdin encoding stays in `base-proof-succinct-host-utils`.
