# Standalone proving

The SP1 standalone prover stack and its `just prover`, `just succinct`, and
`just zk-prover` recipes have been removed. Compressed and SNARK/PLONK proof
requests are unsupported in this prototype.

See [CAVEATS.md](../../CAVEATS.md) for the dependency incompatibility and feature
limitations. For local Nitro/TEE proving, use the
[Docker guide](../../etc/docker/README.md#single-anvil-l1-local-nitro-proving).
