# `base-txmgr`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Transaction lifecycle management for Base onchain components.

## Overview

- **`TxManagerError`**: Error types for transaction management operations.
- **`TxCandidate`**: Represents a candidate transaction to be submitted.
- **`FeeCalculator`**: Calculates and bumps transaction fees.
- **`SendState`**: Tracks the state of a transaction through its lifecycle.
- **`TxManagerConfig`**: Configuration for the transaction manager.
- **`TxManager`**: Trait defining the transaction manager public API.
- **`NonceManager`**: Manages nonce allocation and tracking.
- **`SimpleTxManager`**: Default transaction manager implementation.
- **`TxQueue`**: Queue for ordering and batching transactions.
- **`TxMetrics`**: Metrics collection for transaction operations.
- **`BlobTxBuilder`**: Builder for EIP-4844 blob-carrying transactions.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-txmgr = { git = "https://github.com/base/base" }
```

```rust,ignore
use base_txmgr::{SimpleTxManager, TxCandidate, TxManagerConfig};

// Build a transaction candidate and submit it via the manager
let config = TxManagerConfig;
let manager = SimpleTxManager;
let candidate = TxCandidate;
```

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
