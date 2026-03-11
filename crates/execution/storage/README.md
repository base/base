# `base-execution-storage`

Storage implementation for Base.

## Overview

Defines the `OpStorage` abstraction for chain state persistence in the Base execution node.
Wraps Reth's storage layer with Base-specific configuration including pruning mode
compatibility checks and the storage types needed by the execution engine and provider stack.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-storage = { workspace = true }
```

```rust,ignore
use base_execution_storage::OpStorage;

let storage = OpStorage::new(db_path, prune_config)?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
