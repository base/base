# `base-flashblocks`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Flashblocks state management for Base nodes. Subscribes to flashblocks and combines the state with the canonical block stream to provide a consistent view of pending transactions, blocks, and receipts before they are finalized on-chain.

## Overview

- **`FlashblocksState`**: Core state container that tracks pending blocks and transactions.
- **`FlashblocksSubscriber`**: WebSocket subscriber for receiving flashblock updates from the builder.
- **`StateProcessor`**: Processes incoming flashblocks and produces state updates.
- **`PendingBlocks`**: Manages the collection of pending blocks with builder pattern via `PendingBlocksBuilder`.
- **`PendingStateBuilder`**: Builds pending state from executed transactions.
- **`CanonicalBlockReconciler`**: Reconciles flashblock state with canonical chain updates.
- **`ReorgDetector`**: Detects chain reorganizations affecting pending state.

## RPC Extensions

This crate provides extended Ethereum RPC subscriptions for pending state:

- **`eth_subscribe("newPendingBlock")`**: Subscribe to pending block updates from flashblocks.
- **`eth_subscribe("newPendingReceipts")`**: Subscribe to pending transaction receipts.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-flashblocks = { git = "https://github.com/base/base" }
```

Subscribe to flashblocks and process state updates:

```rust,ignore
use base_flashblocks::{FlashblocksSubscriber, FlashblocksState, StateProcessor};

// Create subscriber and connect to flashblocks endpoint
let subscriber = FlashblocksSubscriber::new(flashblocks_url);

// Process incoming flashblocks
let processor = StateProcessor::new(state.clone());
processor.process(flashblock)?;

// Access pending state
let pending_block = state.pending_block();
let pending_receipts = state.pending_receipts();
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
