# `base-node-core`

Base execution node implementation.

## Overview

Provides the core node type definitions and builder components for the Base execution node. Includes
`BaseEngineTypes` for consensus/execution engine integration, `BaseEngineApiBuilder` for
constructing the Engine API handler, and payload and proof-history types. This crate wires
together the execution layer's engine, RPC, and payload subsystems.

`BaseComponentsBuilder` constructs the Base EVM, transaction pool, network, and consensus directly.
`BasePayloadServiceBuilder` configures either the standard service on a dedicated thread or the
full-block service as a critical task. Both modes use the same component types and extension hooks.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-node-core = { workspace = true }
```

```rust,ignore
use reth_node_builder::NodeBuilder;

let node = NodeBuilder::new(config)
    .with_database(database)
    .with_provider()
    .with_components(components)
    .launch()
    .await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
