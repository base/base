# `base-account-abstraction`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Types and utilities for ERC-4337 account abstraction. Provides types for user operations (v0.6 and v0.7), mempool management, validation, and reputation tracking.

## Overview

- **`VersionedUserOperation`**: Enum supporting both ERC-4337 v0.6 `UserOperation` and v0.7 `PackedUserOperation` formats.
- **`UserOperationRequest`**: A user operation with its associated entry point address and chain ID.
- **`WrappedUserOperation`**: A user operation paired with its computed hash for mempool storage.
- **`Mempool`**: Trait for managing user operations in a mempool.
- **`ReputationService`**: Trait for querying entity reputation status (Ok, Throttled, Banned).
- **`MempoolEvent`**: Events emitted when user operations are added, included, or dropped.
- **`EntryPointVersion`**: Version detection for v0.6 and v0.7 entry point contracts.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-account-abstraction = { git = "https://github.com/base/base" }
```

Parse and hash a user operation:

```rust,ignore
use base_account_abstraction::{UserOperationRequest, VersionedUserOperation, WrappedUserOperation};

// Deserialize a user operation (automatically detects v0.6 or v0.7 format)
let user_op: VersionedUserOperation = serde_json::from_str(json)?;

// Create a request with entry point and chain ID
let request = UserOperationRequest {
    user_operation: user_op,
    entry_point: entry_point_address,
    chain_id: 8453, // Base mainnet
};

// Compute the user operation hash
let hash = request.hash()?;

// Wrap for mempool storage
let wrapped = WrappedUserOperation {
    operation: request.user_operation,
    hash,
};
```

Implement a mempool:

```rust,ignore
use base_account_abstraction::{Mempool, PoolConfig, WrappedUserOperation, UserOpHash};
use std::{collections::{BTreeSet, HashMap}, sync::Arc};

pub struct InMemoryMempool {
    config: PoolConfig,
    by_priority: BTreeSet<PriorityOp>,
    by_hash: HashMap<UserOpHash, WrappedUserOperation>,
}

impl Mempool for InMemoryMempool {
    fn add_operation(&mut self, operation: &WrappedUserOperation) -> Result<(), anyhow::Error> {
        // Validate minimum gas price
        if operation.operation.max_fee_per_gas() < self.config.minimum_max_fee_per_gas {
            return Err(anyhow::anyhow!("Gas price below minimum"));
        }
        // Add to indexes
        self.by_hash.insert(operation.hash, operation.clone());
        Ok(())
    }

    fn get_top_operations(&self, n: usize) -> impl Iterator<Item = Arc<WrappedUserOperation>> {
        // Return top N operations ordered by max_priority_fee_per_gas (descending)
        self.by_priority.iter().take(n).map(|op| Arc::new(op.inner.clone()))
    }

    fn remove_operation(&mut self, hash: &UserOpHash) -> Result<Option<WrappedUserOperation>, anyhow::Error> {
        Ok(self.by_hash.remove(hash))
    }
}
```
