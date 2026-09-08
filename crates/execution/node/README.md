# `base-node-core`

Base execution node implementation.

## Overview

Provides the node types and builders for Base execution. The node owns the execution driver,
payload builder, local provider, proof-history progress, and shutdown lifecycle. Consensus receives
these services directly. Public HTTP/WS RPC is an optional node service.

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
