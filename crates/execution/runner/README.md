# `base-node-runner`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Primitive types and traits for Base node runner extensions. Provides extension hooks and type
aliases for building modular node extensions.

## Overview

- **`BaseNodeExtension`**: Trait for node extensions that can apply additional wiring to the builder.
- **`FromExtensionConfig`**: Trait for extensions that can be constructed from a configuration type.
- **`BaseNodeRunner`**: Orchestrates Base node wiring and launch.
- **`NodeHooks`**: Hook accumulator used by extensions to register RPC and node-start hooks.
- **`BaseNodeBuilder`**: Type alias for the node builder with launch context.
- **`BaseProvider`**: Type alias for the blockchain provider instance.

Configuration types are located in their respective feature crates:
- **`FlashblocksConfig`**: in `base-flashblocks` crate
- **`TxpoolConfig`**: in `base-txpool-tracing` crate

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-node-runner = { git = "https://github.com/base/base" }
```

Implement a custom node extension:

```rust,ignore
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use eyre::Result;

#[derive(Debug)]
struct MyExtension {
    // extension state
}

impl BaseNodeExtension for MyExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        // Register custom hooks
        hooks
    }
}

impl FromExtensionConfig for MyExtension {
    type Config = MyConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { /* ... */ }
    }
}
```

## Frozen payload notifications

`LaunchedBaseNode::payload_ready` is a bounded, in-process EL-to-CL channel carrying
`PayloadReady { payload_id, parent_hash }`. The default payload builder sends a hint only
after a sequencer build returns `Freeze`, including completed state-root computation.
Pre-Denim improvements, failed or cancelled builds, and `no_tx_pool` replay builds do not
send hints. Custom payload services must explicitly wire `BaseNode::payload_ready` if
they support this contract.

The receiver exists before node launch so fast builds cannot race subscription. Sending
never waits for the consumer. The channel retains 16 hints; receivers must handle lag or
closure by retaining their normal seal deadline, and must match both identifiers against
the active build before acting. `getPayload` awaits the completed build even if the hint
arrives just before reth installs its result into the payload job.

The unified sequencer matches these hints against its active Denim build and calls
`getPayload` through the existing seal path immediately. Conductor commit, gossip, and
local insertion keep their existing order. The scheduled seal remains the fallback, and
the next block's build still waits for its parent's scheduled timestamp. Split EL/CL
deployments have no IPC/WS subscription and continue using the scheduled seal.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
