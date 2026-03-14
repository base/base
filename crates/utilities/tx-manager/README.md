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
- **`PreparedTx`**: A signed transaction together with the fee values (`gas_tip_cap`,
  `gas_fee_cap`) that were applied during construction. Returned by `prepare()` and
  `craft_tx()` so callers can track the actual on-wire fees without a redundant gas price
  query.
- **`GasPriceCaps`**: Intermediate fee estimates (tip cap, base fee cap, optional blob fee cap)
  passed between fee calculation and transaction construction.
- **`FeeCalculator`**: Pure, deterministic fee arithmetic engine operating on `u128` values
  with saturating math. Provides EIP-1559 fee cap calculation (`calc_gas_fee_cap`), blob fee
  cap calculation (`calc_blob_fee_cap`), geth-compatible replacement thresholds
  (`calc_threshold_value`), four-case fee update logic (`update_fees`), and configurable fee
  ceiling enforcement (`check_limits`).
- **`SendResponse`**: Type alias (`TxManagerResult<TransactionReceipt>`) returned by async
  send operations.
- **`SendHandle`**: Future returned by `send_async` that resolves directly to a
  `SendResponse`, mapping a closed channel into `TxManagerError::ChannelClosed` so callers
  avoid a two-layer `Result`.
- **`SendState`**: State machine the send loop uses to decide whether to continue retrying,
  bump fees, or abort. Tracks mined transaction hashes, nonce-too-low error counts, mempool
  deadline expiry, successful publish counts, and fee bump state. Uses `std::sync::Mutex`
  for thread-safe interior mutability (all critical sections are CPU-bound with no `.await`
  points). `SendState::new` returns `TxManagerResult<Self>` and rejects a zero
  `safe_abort_nonce_too_low_count` threshold with
  `TxManagerError::InvalidSafeAbortNonceTooLowCount`. The `critical_error()` method
  evaluates abort conditions in priority order: mined tx suppression, already-reserved,
  pre-publish nonce-too-low, threshold nonce-too-low, and mempool deadline expiry.
- **`TxManagerCli`**: Clap-based CLI argument struct with environment variable fallbacks
  (prefix `BASE_TX_MANAGER`). Captures all tunable tx-manager parameters and is designed
  to be `#[command(flatten)]`-ed into parent CLI structs. Generates `Default` and
  `TryFrom<TxManagerCli> for TxManagerConfig` impls.
- **`TxManagerConfig`**: Validated runtime configuration with public fields. Can be
  constructed directly and validated via `validate()`, or converted from `TxManagerCli`
  via `TryFrom`.
- **`ConfigError`**: Validation error enum returned by the `TryFrom<TxManagerCli>`
  conversion when configuration values are out of range or gwei strings are invalid.
- **`GweiParser`**: Unit struct with `parse` method for converting decimal gwei strings
  to `u128` wei via `alloy_primitives::utils::parse_units`.
- **`TxManager`**: Trait defining the public API — `send` (blocking), `send_async` (returns
  a `SendHandle`), and `sender_address`. Requires `Send + Sync`.
- **`NonceManager`**: Manages nonce allocation and tracking with lazy initialization from
  chain state. Wraps a `tokio::sync::Mutex` around a cached nonce, fetching the initial
  value via `get_transaction_count()` on first use and incrementing locally on subsequent
  calls. Returns a [`NonceGuard`] from `next_nonce()` that holds the lock for the duration
  of signing. `reset()` clears the cache, forcing a fresh chain fetch on the next call.
- **`NonceGuard`**: RAII guard holding a reserved nonce and the nonce mutex lock. Drop the
  guard after successful signing to release the lock, or call `rollback()` on failure to
  restore the nonce for reuse. Uses `OwnedMutexGuard` so the guard is `Send` and can cross
  task spawn boundaries.
- **`SimpleTxManager`**: Default `TxManager` implementation. Holds a `RootProvider`,
  `EthereumWallet`, `TxManagerConfig`, `NonceManager`, chain ID, and a shutdown flag.
  `new()` validates the config and cross-checks the chain ID against the provider.
  `prepare()` wraps `craft_tx()` in a `backon` retry loop (up to 30 attempts, 2-second
  fixed delay) that retries only on transient errors and exits immediately when closed.
  Both methods accept optional fee overrides `(tip, fee_cap)` and return a `PreparedTx`
  containing the RLP-encoded raw bytes and the actual fees applied. `craft_tx()` queries
  gas price caps, applies fee overrides as a floor via `max(network_fee, override)`,
  enforces fee limits, builds a `TransactionRequest` with all fields set manually (no
  alloy fillers), estimates or validates gas, assigns a nonce via `NonceManager`, and
  signs via `NetworkWallet`.
  `suggest_gas_price_caps()` queries the provider for tip cap and base fee, enforces
  configured minimums, and returns a `GasPriceCaps`. Blob transactions are not yet
  supported and are rejected with an error.
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

`ChannelClosed` is returned by [`SendHandle`] when the background send task drops
its sender before delivering a result (panic or cancellation). It is non-retryable.

`FeeLimitExceeded` is returned by `FeeCalculator::check_limits` when the proposed fee
exceeds `fee_limit_multiplier × suggested_fee` and the suggested fee is at or above
`fee_limit_threshold`. It is non-retryable.

`InvalidSafeAbortNonceTooLowCount` is returned by `SendState::new` when the
`safe_abort_nonce_too_low_count` threshold is zero. A zero threshold would abort on the very
first nonce-too-low error after a successful publish, making fee bumps impossible. It is
non-retryable.

`RpcErrorClassifier::classify_rpc_error` lowercases the input and matches against known
geth error substrings in a fixed order (e.g., `"replacement transaction underpriced"`
before `"transaction underpriced"`). Unrecognized strings fall through to the
`TxManagerError::Rpc` fallback, which is conservatively treated as retryable — callers
must enforce bounded retry counts.

For custom error matching beyond the built-in classification, use
`RpcErrorClassifier::err_string_contains_any`.

## Configuration

`TxManagerConfig` is the validated runtime configuration. All fields are
public, so you can construct it directly and call `validate()` to check
invariants. Alternatively, convert from `TxManagerCli` via `TryFrom`
which parses CLI/env arguments and validates automatically.

### CLI parsing and validation

`TxManagerCli` is a `clap::Parser` struct designed to be `#[command(flatten)]`-ed
into parent CLI structs. All fields use `BASE_TX_MANAGER_` environment variable
fallbacks. The macro also generates `Default` and
`TryFrom<TxManagerCli> for TxManagerConfig` impls:

```rust,ignore
use base_tx_manager::TxManagerConfig;

base_tx_manager::define_tx_manager_cli!("BASE_TX_MANAGER");

// Parse from CLI args (typically done by the parent binary).
let cli = TxManagerCli::try_parse().unwrap();

let config = TxManagerConfig::try_from(cli)?;
```

### Custom env var prefix

Consumer crates that need a different env var prefix (e.g.
`BASE_CHALLENGER_TX_MANAGER` instead of `BASE_TX_MANAGER`) can invoke
the `define_tx_manager_cli!` macro directly:

```rust,ignore
// In your crate — generates a local `TxManagerCli` with custom env vars.
base_tx_manager::define_tx_manager_cli!("BASE_CHALLENGER_TX_MANAGER");

#[derive(clap::Parser)]
struct Cli {
    #[command(flatten)]
    tx: TxManagerCli,
}

let config = TxManagerConfig::try_from(cli.tx)?;
```

> **Note:** The macro expands to absolute paths (`::clap::Parser` and
> `::humantime::parse_duration`), so consumer crates must add `clap`
> (with `derive` + `env` features) and `humantime` to their own
> `Cargo.toml`.

### Fee limit checks

Use `FeeCalculator::check_limits` with the multiplier and threshold from
the config:

```rust,ignore
FeeCalculator::check_limits(
    proposed_fee,
    suggested_fee,
    config.fee_limit_multiplier,
    config.fee_limit_threshold,
)?;
```

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-tx-manager = { git = "https://github.com/base/base" }
```

```rust,ignore
use std::sync::Arc;

use alloy_network::EthereumWallet;
use alloy_primitives::{bytes, Address, U256};
use alloy_provider::RootProvider;
use base_tx_manager::{BaseTxMetrics, SimpleTxManager, TxCandidate, TxManager, TxManagerConfig};

// Create a SimpleTxManager with a provider, wallet, and config.
let provider = RootProvider::new_http("http://localhost:8545".parse()?);
let wallet = EthereumWallet::from(signer);
let config = TxManagerConfig::default();
let chain_id = 1;
let manager = SimpleTxManager::new(provider, wallet, config, chain_id, Arc::new(BaseTxMetrics)).await?;

// Build a regular (type-2) transaction candidate.
let candidate = TxCandidate {
    tx_data: bytes!("deadbeef"),
    to: Some(Address::ZERO),
    gas_limit: 21_000,
    value: U256::from(1_000),
    ..Default::default()
};

// Construct and sign the transaction (with automatic retry on transient errors).
// Returns a PreparedTx containing the raw bytes and the fees that were applied.
let prepared = manager.prepare(&candidate, None).await?;
let raw_tx = prepared.raw_tx;

// Or use craft_tx() directly for a single attempt without retry.
let prepared = manager.craft_tx(&candidate, None).await?;
let raw_tx = prepared.raw_tx;
```

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
