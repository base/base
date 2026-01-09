# `base-reth-runner`

<a href="https://github.com/base/node-reth/actions/workflows/ci.yml"><img src="https://github.com/base/node-reth/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/node-reth/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Base-specific node launcher that wires together the Optimism node components, execution extensions, and RPC add-ons for the Base node binary. Exposes the types that the CLI uses to build a node and pass them to Optimism's `Cli` runner.

## Overview

- **`BaseNodeBuilder`**: Builder for constructing a Base node with custom extensions and configuration.
- **`BaseNodeRunner`**: Runs the Base node with all configured extensions.
- **`BaseNodeHandle`**: Handle to the running node, providing access to providers and state.
- **`BaseNodeConfig`**: Configuration options for the Base node.
- **`FlashblocksConfig`**: Configuration for flashblocks WebSocket subscription.
- **`TracingConfig`**: Configuration for transaction tracing extension.

## Extensions

- **`BaseNodeExtension`**: Core extension trait for adding functionality to the node.
- **`BaseRpcExtension`**: Extension for adding custom RPC methods.
- **`FlashblocksCanonExtension`**: Extension for canonical block reconciliation with flashblocks.
- **`TransactionTracingExtension`**: Extension for transaction lifecycle tracing.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-reth-runner = { git = "https://github.com/base/node-reth" }
```

Build and run a Base node:

```rust,ignore
use base_reth_runner::{BaseNodeBuilder, BaseNodeConfig, FlashblocksConfig};

let config = BaseNodeConfig {
    flashblocks: FlashblocksConfig {
        enabled: true,
        url: "wss://flashblocks.base.org".to_string(),
    },
    ..Default::default()
};

let node = BaseNodeBuilder::new(config)
    .with_flashblocks()
    .with_transaction_tracing()
    .build()
    .await?;
```

## License

Licensed under the [MIT License](https://github.com/base/node-reth/blob/main/LICENSE).
