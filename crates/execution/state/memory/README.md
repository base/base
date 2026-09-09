# `base-execution-state-memory`

Accounts, storage, memory databases, execution caches, state transitions, and rollback for Base
execution and proofs. This combines the Base database layer and locally maintained REVM state.

`Account` carries execution journal state, including loaded storage and `JournalAccountStatus`.
`AccountInfo` carries account metadata. `StoredAccount` and `StoredBytecode` preserve the compact
records used by persistent storage; `reth-codec` enables those encodings. `AccountStatus` tracks
the longer-lived bundle transition lifecycle.

`Database`, `DatabaseRef`, and commit interfaces connect the interpreter to storage. In-memory
implementations, cache overlays, block access lists, bundle transitions, and reverts live here.
Persistent providers and network clients remain outside this crate.

The crate supports `no_std` with allocation. Serialization and persisted codecs are independently
selected features.

```sh
cargo test -p base-execution-state-memory --features reth-codec
cargo check -p base-execution-state-memory --no-default-features --target riscv32imac-unknown-none-elf
```
