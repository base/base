# `base-execution-state-types`

Shared execution-state and trie data: hashed accounts and storage, trie updates, proof targets,
intermediate hash-builder state, persisted account and storage changes, block indices, pruning policies, pipeline checkpoints and targets, static-file metadata, storage errors, execution outcomes and chain segments, and serialization helpers.

These types are used by persistent providers, trie computation, synchronization, and execution
consumers. Memory state is provided by `base-execution-state-memory`; provider and database
implementations remain above this crate.

`reth-codec` retains persisted encodings, `serde-bincode-compat` enables stable binary serialization
helpers, and `test-utils` enables property-test fixtures. Allocation-only builds remain supported.

```sh
cargo test -p base-execution-state-types --features reth-codec,serde-bincode-compat,test-utils
cargo check -p base-execution-state-types --no-default-features --target riscv32imac-unknown-none-elf
```
