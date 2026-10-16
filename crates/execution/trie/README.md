# `base-execution-trie`

Trie implementation for Base.

## Overview

Manages Merkle Patricia Trie proof storage for the fault-proof window. The `BaseProofsStore`
traits and storage backends accumulate per-block state diffs and trie node preimages, making them
available for proof generation without re-executing blocks. Provides cursor interfaces for
navigating account and storage tries, a pruner for removing data outside the retention window, and
an initialization job for syncing historical proofs at startup.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-trie = { workspace = true }
```

```rust,ignore
use std::{path::Path, sync::Arc, time::Duration};

use base_execution_trie::{
    BaseProofStoragePrunerTask, BaseProofsStorage, BaseProofsStorageError, RocksdbProofsStorage,
};
use reth_provider::BlockHashReader;
use reth_tasks::shutdown::GracefulShutdown;

async fn run_pruner<H: BlockHashReader + Send + Sync + 'static>(
    db_path: &Path,
    hash_reader: H,
    retention_blocks: u64,
    interval: Duration,
    signal: GracefulShutdown,
) -> Result<(), BaseProofsStorageError> {
    let provider: BaseProofsStorage<Arc<RocksdbProofsStorage>> =
        Arc::new(RocksdbProofsStorage::new(db_path)?).into();
    BaseProofStoragePrunerTask::new(provider, hash_reader, retention_blocks, interval)
        .run(signal)
        .await;
    Ok(())
}
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
