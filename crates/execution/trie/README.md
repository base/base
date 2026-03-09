# `base-execution-trie`

Trie implementation for Base.

## Overview

Manages Merkle Patricia Trie proof storage for the fault-proof window. The `OpProofsStorage`
type (backed by either in-memory or MDBX) accumulates per-block state diffs and trie node
preimages, making them available for proof generation without re-executing blocks. Provides
cursor interfaces for navigating account and storage tries, a pruner for removing data outside
the retention window, and an initialization job for syncing historical proofs at startup.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-trie = { workspace = true }
```

```rust,ignore
use base_execution_trie::{MdbxProofsStorage, OpProofStoragePruner};

let storage = MdbxProofsStorage::open(db_path)?;
let pruner = OpProofStoragePruner::new(storage.clone(), retention_blocks);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
