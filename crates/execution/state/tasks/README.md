# `base-execution-state-tasks`

Trie implementation for Base.

## Overview

Manages Merkle Patricia Trie proof storage for the fault-proof window. The `BaseProofsStore`
traits and storage backends accumulate per-block state diffs and trie node preimages, making them
available for proof generation without re-executing blocks. Provides cursor interfaces for
navigating account and storage tries, a pruner for removing data outside the retention window, and
an initialization job for syncing historical proofs at startup.

Also owns parallel account/storage proof workers and the state-root task handles shared by the
Engine API and payload builder. Core trie algorithms and database cursors live in `reth-trie`,
below the provider layer; this crate composes providers with task scheduling and proof history.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-state-tasks = { workspace = true }
```

```rust,ignore
use base_execution_state_tasks::{BaseProofStoragePruner, RocksdbProofsStorage};

let storage = RocksdbProofsStorage::new(db_path)?;
let pruner = BaseProofStoragePruner::new(storage.clone(), retention_blocks);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
