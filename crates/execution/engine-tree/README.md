# `base-engine-tree`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Base's implementation of the engine tree validator, responsible for block execution and state root computation within the Reth engine tree.

## Overview

This crate provides the core block validation pipeline for the Base node. It implements Reth's `EngineValidator` trait and orchestrates the full lifecycle of validating a new block or payload: consensus checks, EVM execution, receipt root computation, and state root verification.

### Key Components

- **`BaseEngineValidator`**: The primary validator that coordinates end-to-end block validation. It handles:
  - Payload-to-block conversion
  - EVM environment setup
  - Block execution with precompile caching
  - Parallel and async state root computation
  - Post-execution consensus validation
  - Invalid block hook invocation

- **`CachedExecutor`**: A block executor wrapper that attempts to reuse cached execution results from a `CachedExecutionProvider` (e.g., from flashblocks). Falls back to full EVM execution when cache misses occur.

- **`CachedExecutionProvider`**: A trait for providers that supply pre-computed transaction execution results, enabling skip-ahead execution when prior results are available.

- **`NoopCachedExecutionProvider`**: A default no-op implementation that always returns `None`, forcing full execution for every transaction.

## Features

- **State Root Strategies**: Supports three state root computation strategies that are chosen based on configuration:
  - **`StateRootTask`**: Background sparse trie computation with proof generation
  - **Parallel**: Multi-threaded state root computation via `ParallelStateRoot`
  - **Synchronous**: Serial fallback for testing or when parallel approaches fail

- **Incremental Receipt Root**: Spawns a background task that computes the receipt root and logs bloom incrementally as transactions execute, overlapping I/O with computation.

- **Precompile Caching**: Wraps EVM precompiles with a shared cache to avoid redundant computation across blocks.

- **Lazy Trie Overlays**: Constructs `LazyOverlay` instances that defer expensive trie input merging until first access, allowing execution to start immediately.

- **Deferred Trie Tasks**: After validation, spawns a background task to sort and merge trie updates and changesets so the validation hot path returns without blocking.

- **Cached Execution**: Integrates with `CachedExecutionProvider` to skip EVM re-execution for transactions whose results are already known (e.g., from flashblock pre-validation).

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-engine-tree = { git = "https://github.com/base/base" }
```

### Constructing the Validator

`BaseEngineValidator` requires a provider, consensus engine, EVM config, payload validator, and a cached execution provider:

```rust,ignore
use base_engine_tree::BaseEngineValidator;

let validator = BaseEngineValidator::new(
    provider,
    consensus,
    evm_config,
    payload_validator,
    tree_config,
    invalid_block_hook,
    cached_execution_provider,
    changeset_cache,
    runtime,
);
```

### Custom Cached Execution

Implement `CachedExecutionProvider` to supply pre-computed execution results:

```rust,ignore
use base_engine_tree::CachedExecutionProvider;

#[derive(Debug, Clone)]
struct MyFlashblockCache { /* ... */ }

impl CachedExecutionProvider<OpTxResult<OpHaltReason, OpTxType>> for MyFlashblockCache {
    fn get_cached_execution_for_tx(
        &self,
        parent_block_hash: &B256,
        prev_cached_hash: Option<&B256>,
        tx_hash: &B256,
    ) -> Option<OpTxResult<OpHaltReason, OpTxType>> {
        // Look up cached result from flashblock execution
        self.cache.get(start_state_root, tx_hash)
    }
}
```

If no caching is needed, use `NoopCachedExecutionProvider`.

## Architecture

The validator follows a pipelined architecture for block validation:

1. **State Resolution**: Resolves the parent block's state from in-memory tree state or the database via `StateProviderBuilder`
2. **Execution Planning**: Selects a state root computation strategy and spawns the appropriate payload processor
3. **Block Execution**: Runs EVM execution with precompile caching, streaming receipts to a background receipt root task
4. **Post-Execution Validation**: Validates consensus rules, header-against-parent, receipt root, and hashed post-state
5. **State Root Verification**: Awaits parallel/async state root computation (with timeout fallback to serial) and verifies against the block header
6. **Deferred Trie Task**: Spawns a background task to compute sorted trie data and changesets for downstream consumers

This design maximizes parallelism — execution, receipt root computation, state root computation, and trie data preparation all overlap where possible.

## Dependencies

This crate builds on top of several Reth and OP Stack components:
- `reth-engine-tree`: Engine tree traits, state management, and payload processing
- `reth-consensus`: Consensus validation rules
- `reth-evm`: EVM configuration and block execution
- `reth-provider`: State and storage access
- `reth-trie` / `reth-trie-parallel`: Trie computation (serial and parallel)
- `base-execution-evm`: OP Stack EVM specializations

## Related Crates

- **`base-engine`**: Engine validator builder that constructs `BaseEngineValidator` instances
- **`base-flashblocks`**: Provides cached execution results that integrate with `CachedExecutor`
- **`base-client-node`**: Node builder extensions that wire up the full validation pipeline
