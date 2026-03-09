# `base-execution-node`

Base execution node implementation.

## Overview

Provides the core node type definitions and builder components for the Base execution node. Includes
`OpEngineTypes` for consensus/execution engine integration, `OpEngineApiBuilder` for constructing
the Engine API handler, `OpStorage` for chain state persistence, and payload builder and attestor
types. This crate wires together the execution layer's engine, RPC, and payload subsystems.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-node = { workspace = true }
```

```rust,ignore
use base_execution_node::{OpEngineTypes, OpEngineApiBuilder};

let node = NodeBuilder::new(config)
    .with_types::<OpEngineTypes>()
    .with_components(components)
    .launch()
    .await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
