# `base-client-runner`

<a href="https://github.com/base/node-reth/actions/workflows/ci.yml"><img src="https://github.com/base/node-reth/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/node-reth/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Base-specific node launcher that wires together the Optimism node components, execution extensions, and RPC add-ons for the Base node binary. Exposes the types that the CLI uses to build a node and pass them to Optimism's `Cli` runner.

## Overview

- **`BaseNodeBuilder`**: Builder for constructing a Base node with custom extensions and configuration.
- **`BaseNodeRunner`**: Runs the Base node with all configured extensions.
- **`BaseNodeHandle`**: Handle to the running node, providing access to providers and state.
- **`BaseNodeConfig`**: Configuration options for the Base node.

## Extensions

Each feature crate provides a single `BaseNodeExtension` that combines all functionality for that feature:

- **`FlashblocksExtension`** (from `base-flashblocks`): Combines canon ExEx and RPC for flashblocks.
- **`TxPoolExtension`** (from `base-txpool`): Combines transaction tracing ExEx and status RPC.
- **`MeteringExtension`** (from `base-metering`): Provides metering RPC.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-client-runner = { git = "https://github.com/base/node-reth" }
```

Build and run a Base node:

```rust,ignore
use base_client_runner::BaseNodeRunner;
use base_flashblocks::FlashblocksExtension;
use base_metering::MeteringExtension;
use base_txpool::TxPoolExtension;

let mut runner = BaseNodeRunner::new(args);
runner.install_ext::<TxPoolExtension>()?;
runner.install_ext::<MeteringExtension>()?;
runner.install_ext::<FlashblocksExtension>()?;

let handle = runner.run(builder);
```

## License

Licensed under the [MIT License](https://github.com/base/node-reth/blob/main/LICENSE).
