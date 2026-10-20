# `base-tx-manager`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Transaction lifecycle management for Base onchain components.

## Overview

Manages the full lifecycle of EIP-1559 and EIP-4844 blob transactions: fee
estimation, construction, signing, submission, fee bumping, and receipt
confirmation.

### Core types

- **`TxManager`** — trait defining the public API (`send`, `send_async`, `sender_address`).
- **`SimpleTxManager`** — default `TxManager` implementation. Handles gas estimation,
  nonce management, signing, submission with fee bumps, and receipt polling.
- **`TxCandidate`** — input to the send pipeline (calldata, recipient, gas limit, value,
  optional blobs).
- **`PreparedTx`** — signed transaction bytes and the fees, gas limit, nonce, and sidecar
  that were applied.

### Fee calculation

- **`FeeCalculator`** — pure arithmetic for EIP-1559/EIP-4844 fee caps, replacement
  thresholds, fee bumps, and fee limit enforcement.
- **`GasPriceCaps`** — intermediate fee estimates passed between estimation and construction.
- **`FeeOverride`** — optional per-field fee and gas-limit overrides applied as a floor.
- **`BumpedFees`** — result of a fee bump calculation.

### Error handling

- **`TxManagerError`** — error variants categorized as critical (non-retryable),
  fee/replacement (retryable via fee bumps), or infrastructure (transient/retryable).
  `NonceTooHigh` is transient because concurrent publications can reach RPC
  backends out of order; the same signed transaction is retried.
- **`RpcErrorClassifier`** — classifies alloy `TransportError`s into `TxManagerError` variants.
- **`RevertDisplay`** — display wrapper for decoded revert reasons.
- **`TxManagerResult<T>`** — result type alias.

`SimpleTxManager::send_tracked(candidate, &published)` uses the same send pipeline as `TxManager::send` and retains every attempted signed hash in a caller-owned `PublishedTransactions` journal. Read `published.hashes()` after an error, including `SendTimeout`, or after cancelling the send future. Hashes are recorded before broadcast and deduplicated in attempt order. A recorded hash may not have reached the network, but an error does not prove it was rejected. Use a fresh journal for each logical send. Existing error variants and retry classification are unchanged. A successful send returns its confirmed receipt even when its status is zero; callers must check receipt status.

For tracked sends, an already-known broadcast response counts as success even on the initial attempt, since a transport may have replayed a request that the node accepted. Untracked sends accept it only after a prior successful publication; on the initial broadcast it remains a retryable error returned to the caller, and the signed nonce is returned for reuse. Publication returns the submitted transaction's local hash, not a previous replacement's hash.

For tracked sends, any other JSON-RPC broadcast rejection triggers one lookup of the local hash, bounded by `network_timeout`. If the node knows the transaction, publication counts as successful and normal receipt confirmation continues. A missing, failed, or timed-out lookup preserves the original rejection. Reconciled nonces are not returned to the reuse pool after a later send error.

Cancelling a send future does not reconcile the nonce cache. Serialized callers using `with_pending_nonce()` can reset the nonce manager before their next send to refresh the node's pending nonce. Do not reset it while concurrent sends are allocating nonces.

### Nonce management

- **`NonceManager`** — lazy-initialized nonce cache with mutex-guarded allocation.
- **`NonceGuard`** — RAII guard holding a reserved nonce; drop consumes it, or
  call `rollback()` on failure to return it for reuse.
- **`NonceState`** — snapshot of the current nonce state.

### Send state

- **`SendState`** — state machine tracking mined hashes, nonce errors, fee bumps, and
  mempool deadlines to decide whether to retry, bump, or abort.
- **`SendHandle`** — future returned by `send_async` that resolves to a `SendResponse`.
- **`SendResponse`** — type alias for `TxManagerResult<TransactionReceipt>`.

### Blob support

- **`BlobTxBuilder`** — builds EIP-7594 cell-proof sidecars from raw blobs.
- **`MAX_BLOBS_PER_TX`** — maximum blobs per transaction (Fusaka limit).

### Config types

- **`TxManagerConfig`** — validated runtime configuration with public fields.
- **`ConfigError`** — validation error for out-of-range or invalid config values.
- **`GweiParser`** — converts decimal gwei strings to `u128` wei.
- **`define_tx_manager_cli!`** — macro that generates a `TxManagerCli` clap struct with
  env var fallbacks and `TryFrom<TxManagerCli> for TxManagerConfig`.
- **`define_signer_cli!`** — macro that generates a signer CLI struct.
- **`SignerConfig`** — describes wallet construction (local `PrivateKeySigner` or remote
  signer endpoint).

### Metrics

- **`TxMetrics`** — trait for transaction metrics collection.
- **`BaseTxMetrics`** — production implementation backed by the `metrics` crate.
- **`NoopTxMetrics`** — no-op implementation for tests.
- **`TxManagerMetrics`** — raw metrics struct generated by `define_metrics!`.

## Error Classification

`TxManagerError` variants are classified to let the send loop decide whether to retry,
bump fees, or abort. Use `is_retryable()` and `is_already_known()` to branch:

```rust,ignore
use base_tx_manager::{RpcErrorClassifier, TxManagerError};

fn handle_rpc_error(transport_err: &alloy_transport::TransportError) {
    let err = RpcErrorClassifier::classify_rpc_error(transport_err);
    if err.is_already_known() {
        // Accept on a tracked send or after a prior successful publication.
    } else if err.is_retryable() {
        // Fee/replacement errors and transient infrastructure errors — retry with backoff.
    } else {
        // Critical errors (nonce conflicts, insufficient funds, reverts) — abort.
    }
}
```

## Configuration

`TxManagerConfig` can be constructed directly and validated via `validate()`, or
converted from a macro-generated `TxManagerCli` via `TryFrom`.

### CLI parsing

```rust,ignore
use base_tx_manager::TxManagerConfig;

base_tx_manager::define_tx_manager_cli!("BASE_TX_MANAGER");

let cli = TxManagerCli::try_parse().unwrap();
let config = TxManagerConfig::try_from(cli)?;
```

### Custom env var prefix

```rust,ignore
base_tx_manager::define_tx_manager_cli!("BASE_CHALLENGER_TX_MANAGER");

#[derive(clap::Parser)]
struct Cli {
    #[command(flatten)]
    tx: TxManagerCli,
}

let config = TxManagerConfig::try_from(cli.tx)?;
```

> **Note:** Consumer crates must add `clap` (with `derive` + `env` features) and
> `humantime` to their own `Cargo.toml`.

## Usage

```toml
[dependencies]
base-tx-manager = { git = "https://github.com/base/base" }
```

```rust,ignore
use std::sync::Arc;

use alloy_primitives::{bytes, Address, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::{
    BaseTxMetrics, SignerConfig, SimpleTxManager, TxCandidate, TxManagerConfig,
};

let provider = RootProvider::new_http("http://localhost:8545".parse()?);
let signer_config = SignerConfig::local("0x0101010101010101010101010101010101010101010101010101010101010101".parse::<PrivateKeySigner>()?);
let config = TxManagerConfig::default();
let metrics = Arc::new(BaseTxMetrics::new("my_service"));
let manager = SimpleTxManager::new(provider, signer_config, config, 1, metrics).await?;

// Build a regular (type-2) transaction candidate.
let candidate = TxCandidate {
    tx_data: bytes!("deadbeef"),
    to: Some(Address::ZERO),
    gas_limit: 21_000,
    value: U256::from(1_000),
    ..Default::default()
};

// Construct and sign with automatic retry on transient errors.
let prepared = manager.prepare(&candidate, None).await?;
let raw_tx = prepared.raw_tx;

// Or use craft_tx() for a single attempt without retry.
let prepared = manager.craft_tx(&candidate, None).await?;
```

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
