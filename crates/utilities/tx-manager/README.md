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
- **`TxCandidate`**: Input to the send pipeline. With empty `blobs` it produces a regular
  EIP-1559 (type-2) transaction; with non-empty `blobs` it produces an EIP-4844 (type-3)
  blob-carrying transaction. Carries calldata, optional recipient, gas limit, and value.
- **`GasPriceCaps`**: Intermediate fee estimates (tip cap, base fee cap, optional blob fee cap)
  passed between fee calculation and transaction construction.
- **`FeeCalculator`**: Calculates and bumps transaction fees.
- **`SendResponse`**: Type alias (`TxManagerResult<TransactionReceipt>`) returned by async
  send operations.
- **`SendState`**: Tracks the state of a transaction through its lifecycle.
- **`TxManagerConfig`**: Configuration for the transaction manager.
- **`TxManager`**: Trait defining the public API — `send` (blocking), `send_async` (returns
  a `oneshot::Receiver<SendResponse>`), and `sender_address`. Requires `Send + Sync`.
- **`NonceManager`**: Manages nonce allocation and tracking.
- **`SimpleTxManager`**: Default `TxManager` implementation.
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
use alloy_primitives::{bytes, Address, U256};
use base_tx_manager::{SimpleTxManager, TxCandidate, TxManager};

// Build a regular (type-2) transaction candidate.
let candidate = TxCandidate {
    tx_data: bytes!("deadbeef"),
    to: Some(Address::ZERO),
    gas_limit: 21_000,
    value: U256::from(1_000),
    ..Default::default()
};

// Blob (type-3) candidates set the `blobs` field instead.
// let blob_candidate = TxCandidate { blobs: vec![blob], ..Default::default() };

// Submit through the manager trait.
let receipt = manager.send(candidate).await?;
```

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
