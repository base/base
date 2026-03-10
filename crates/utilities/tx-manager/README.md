# `base-tx-manager`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Transaction lifecycle management for Base onchain components.

## Overview

- **`TxManagerError`**: Error types for transaction management, categorized as critical
  (non-retryable), fee/replacement (retryable via fee bumps), and infrastructure (transient/retryable).
- **`RpcErrorClassifier`**: Maps raw RPC/geth error strings into structured `TxManagerError` variants
  using ordered substring matching.
- **`TxManagerResult<T>`**: Result type alias for transaction manager operations.
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

## Error Handling

`TxManagerError` variants are classified to let the send loop decide whether to retry,
bump fees, or abort. Use `is_retryable()` and `is_already_known()` to branch on error
type:

```rust,ignore
use base_tx_manager::{RpcErrorClassifier, TxManagerError};

fn handle_rpc_error(raw_msg: &str) {
    let err = RpcErrorClassifier::classify_rpc_error(raw_msg);
    if err.is_already_known() {
        // Transaction is already in the mempool — treat as success on resubmission.
    } else if err.is_retryable() {
        // Fee/replacement errors and transient infrastructure errors — retry with backoff.
    } else {
        // Critical errors (nonce conflicts, insufficient funds, reverts) — abort.
    }
}
```

`RpcErrorClassifier::classify_rpc_error` lowercases the input and matches against known
geth error substrings in a fixed order (e.g., `"replacement transaction underpriced"`
before `"transaction underpriced"`). Unrecognized strings fall through to the
`TxManagerError::Rpc` fallback, which is conservatively treated as retryable — callers
must enforce bounded retry counts.

For custom error matching beyond the built-in classification, use
`RpcErrorClassifier::err_string_contains_any`.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-tx-manager = { git = "https://github.com/base/base" }
```

```rust,ignore
use base_tx_manager::{SimpleTxManager, TxCandidate, TxManagerConfig};

// Build a transaction candidate and submit it via the manager
let config = TxManagerConfig;
let manager = SimpleTxManager;
let candidate = TxCandidate;
```

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
