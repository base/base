# `base-client-engine`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Custom engine validator implementation optimized for validating canonical blocks in the Base node.

## Overview

This crate provides a specialized engine validator that integrates with Reth's engine tree and consensus system. It is optimized for validating canonical blocks after flashblock validation and serves as a building block for Base-specific consensus and execution logic.

### Key Components

- **`BaseEngineValidator`**: A wrapper around Reth's `BasicEngineValidator` that provides reusable payload validation logic. It handles:
  - Consensus validation
  - Block execution
  - State root computation
  - Fork detection

- **`BaseEngineValidatorBuilder`**: A builder that constructs `BaseEngineValidator` instances with the necessary context (provider, consensus engine, EVM config, payload validator, and invalid block hooks).

## Features

- **Payload Validation**: Validates execution payloads from the Engine API against the current chain state
- **Block Validation**: Validates pre-constructed blocks (e.g., from flashblocks) efficiently
- **State Management**: Computes and verifies state roots during block execution
- **Invalid Block Handling**: Integrates with Reth's invalid block hooks for debugging and analysis

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-client-engine = { git = "https://github.com/base/base" }
```

### Using with Node Builder

The validator builder integrates with Reth's node builder pattern:

```rust,ignore
use base_client_engine::BaseEngineValidatorBuilder;
use reth_node_builder::NodeBuilder;

// Create a validator builder with your custom payload validator
let validator_builder = BaseEngineValidatorBuilder::new(my_payload_validator_builder);

// The node builder will automatically use this to construct the engine validator
let node = NodeBuilder::new()
    .with_engine_validator_builder(validator_builder)
    .build()
    .await?;
```

### Custom Payload Validation

Implement custom payload validation logic by providing your own `PayloadValidatorBuilder`:

```rust,ignore
use base_client_engine::BaseEngineValidatorBuilder;
use reth_engine_primitives::PayloadValidator;
use reth_node_builder::rpc::PayloadValidatorBuilder;

#[derive(Debug, Clone)]
struct MyPayloadValidatorBuilder;

impl<Node> PayloadValidatorBuilder<Node> for MyPayloadValidatorBuilder
where
    Node: FullNodeComponents,
{
    type Validator = MyPayloadValidator;

    async fn build(self, ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        Ok(MyPayloadValidator::new(/* ... */))
    }
}

// Use with the base engine validator builder
let engine_validator_builder = BaseEngineValidatorBuilder::new(MyPayloadValidatorBuilder);
```

## Architecture

The validator follows a layered architecture:

1. **Builder Layer** (`BaseEngineValidatorBuilder`): Constructs validators with the necessary dependencies
2. **Validation Layer** (`BaseEngineValidator`): Coordinates validation, execution, and state computation
3. **Integration Layer**: Implements `EngineValidator` trait to work with Reth's engine tree

This design allows for composable, network-specific validation logic while reusing common execution and state management code.

## Dependencies

This crate builds on top of several Reth components:
- `reth-engine-tree`: Engine tree primitives and traits
- `reth-consensus`: Consensus validation
- `reth-evm`: EVM execution configuration
- `reth-provider`: State and storage access
- `reth-payload-primitives`: Payload types and validation

## Related Crates

- **`base-flashblocks`**: Provides optimized block building that works with this validator
- **`base-client-node`**: Node builder extensions that integrate this validator
- **`reth-engine-tree`**: Core engine tree implementation
