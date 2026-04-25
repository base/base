# `base-execution-node`

Base execution node implementation.

## Overview

Provides the core node type definitions and builder components for the Base execution node. Includes
`BaseEngineTypes` for consensus/execution engine integration, `BaseEngineApiBuilder` for
constructing the Engine API handler, and payload and proof-history types. This crate wires
together the execution layer's engine, RPC, and payload subsystems.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-node = { workspace = true }
```

```rust,ignore
use base_execution_node::{BaseEngineApiBuilder, BaseEngineTypes};

let node = NodeBuilder::new(config)
    .with_types::<BaseEngineTypes>()
    .with_components(components)
    .launch()
    .await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
