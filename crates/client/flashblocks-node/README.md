# `base-flashblocks-node`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

## Extension

This crate provides the Flashblocks extension for the Base node, which enables real-time streaming of pending block state and extended RPC capabilities.

## Usage

### Programmatic Integration

To integrate the Flashblocks extension into your node, use the `install_ext` method on your `BaseNodeRunner`:

```rust
use base_client_node::BaseNodeRunner;
use base_flashblocks_node::{FlashblocksConfig, FlashblocksExtension};

let mut runner = BaseNodeRunner::new(rollup_args);

// Create flashblocks configuration
let flashblocks_config: Option<FlashblocksConfig> = args.into();

// Install the flashblocks extension (should be installed last as it uses replace_configured)
runner.install_ext::<FlashblocksExtension>(flashblocks_config);

let handle = runner.run(builder);
```

### CLI Arguments

When running the node binary, Flashblocks can be configured with the following CLI arguments:

- `--websocket-url <WEBSOCKET_URL>`: The WebSocket URL to stream flashblock updates from (required to enable Flashblocks)
- `--max-pending-blocks-depth <MAX_PENDING_BLOCKS_DEPTH>`: Maximum number of pending flashblocks to retain in memory (default: 3)

### Example

```bash
# Run the Base node with Flashblocks enabled
base-node \
  --websocket-url ws://flashblock-service:8080 \
  --max-pending-blocks-depth 5
```

## What It Does

The `FlashblocksExtension` wires up:

1. **State Processor**: Maintains pending block state by processing incoming flashblocks
2. **Canonical Subscription**: Reconciles pending state with canonical blocks
3. **RPC Extensions**: Provides extended Ethereum RPC methods with flashblock awareness
4. **WebSocket Subscriber**: Connects to and streams updates from the flashblock service

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
