# base-reth-mem-db

Pure-Rust in-memory [`Database`] implementation backed by `BTreeMap`.

Designed for WASM targets where the default reth storage backend (MDBX, which
requires `mmap` and native file locking) is unavailable.

Data lives only for the lifetime of the `MemDb` handle — no durable persistence.
