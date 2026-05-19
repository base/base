# `base-execution-trie`

Trie implementation for Base.

## Overview

Manages Merkle Patricia Trie proof storage for the fault-proof window. The `BaseProofsStore`
traits and storage backends accumulate per-block state diffs and trie node preimages, making them
available for proof generation without re-executing blocks. Provides cursor interfaces for
navigating account and storage tries, a pruner for removing data outside the retention window, and
an initialization job for syncing historical proofs at startup.

The RocksDB backend uses the V2 proofs schema: latest account/storage leaves and trie nodes are
kept in current-state column families, while before-change history rows are retained for older
blocks and indexed by per-block change sets for prune and unwind. Legacy RocksDB V1 databases are
not migrated in place; open them with a fresh proofs-history path and rebuild the proof history.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-trie = { workspace = true }
```

```rust,ignore
use base_execution_trie::{BaseProofStoragePruner, RocksdbProofsStorage};

let storage = RocksdbProofsStorage::new(db_path)?;
let pruner = BaseProofStoragePruner::new(storage.clone(), retention_blocks);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
