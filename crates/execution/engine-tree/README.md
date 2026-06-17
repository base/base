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

## Features

- **State Root Strategies**: Supports three state root computation strategies that are chosen based on configuration:
  - **`StateRootTask`**: Background sparse trie computation with proof generation
  - **Parallel**: Multi-threaded state root computation via `ParallelStateRoot`
  - **Synchronous**: Serial fallback for testing or when parallel approaches fail

- **Incremental Receipt Root**: Spawns a background task that computes the receipt root and logs bloom incrementally as transactions execute, overlapping I/O with computation.

- **Precompile Caching**: Wraps EVM precompiles with a shared cache to avoid redundant computation across blocks.

- **Lazy Trie Overlays**: Constructs `LazyOverlay` instances that defer expensive trie input merging until first access, allowing execution to start immediately.

- **Deferred Trie Tasks**: After validation, spawns a background task to sort and merge trie updates and changesets so the validation hot path returns without blocking.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-engine-tree = { git = "https://github.com/base/base" }
```

### Constructing the Validator

`BaseEngineValidator` requires a provider, consensus engine, EVM config, and payload validator:

```rust,ignore
use base_engine_tree::BaseEngineValidator;

let validator = BaseEngineValidator::new(
    provider,
    consensus,
    evm_config,
    payload_validator,
    tree_config,
    invalid_block_hook,
    changeset_cache,
    runtime,
);
```

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

This crate builds on top of several Reth and Base components:
- `reth-engine-tree`: Engine tree traits, state management, and payload processing
- `reth-consensus`: Consensus validation rules
- `reth-evm`: EVM configuration and block execution
- `reth-provider`: State and storage access
- `reth-trie` / `reth-trie-parallel`: Trie computation (serial and parallel)
- `base-execution-evm`: Base EVM specializations

## Related Crates

- **`base-engine`**: Engine validator builder that constructs `BaseEngineValidator` instances
- **`base-client-node`**: Node builder extensions that wire up the full validation pipeline

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
