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

## Pending state

Published pending state either tracks flashblocks built on the node's current
canonical tip, or it is absent until it can. After a delay, overload, or
restart, the processor does not keep a lagged overlay live for RPC.

`max_pending_blocks_depth` is the maximum number of **blocks** the pending
snapshot may extend beyond the provider's canonical tip. The node CLI default
is `3` (canonical `N` may have pending `N+1` through `N+3`). If pending is not
rooted at the current tip hash, or that depth is exceeded, the processor
clears the snapshot and resumes only from an index-0 flashblock whose parent
is the current tip.

This depth is not a count of flashblock messages, and it is not the cache
window. `FlashblockCache` may retain unpublished payloads a few blocks further
ahead so they can be replayed after the matching canonical block arrives.

## Recovery

Canonical blocks and flashblocks share one bounded processor queue. Each update
is stamped with a recovery epoch. If the queue is full, the epoch advances and
pending is cleared; in-flight work from the previous epoch is not published.

Every update is preflighted against the provider's current canonical header:

- **EnterRecovery** clears pending when it cannot safely track the tip.
- **ResumeRecovery** accepts an index-0 flashblock whose parent hash is the
  current tip and rebuilds pending from there.
- **Skip** drops a stale update when pending is already tip-aligned.
- **Process** applies the update when pending already tracks the tip.

A canonical notification can arrive before the provider exposes that block.
The processor stores it as a deferred canonical, keeps nearby cache entries,
and does not resume against the stale provider tip. A short retry reapplies
the deferred block once it is visible.

When a canonical block is applied, [`CanonicalBlockReconciler`] chooses:

- **Keep** for untracked older canonicals so they are not treated as reorgs.
- **Rebase** if canonical sits inside the pending chain and is still behind
  latest: drop flashblocks at or below that height and rebuild the suffix.
- **CatchUp** if canonical has reached or passed pending latest.
- **HandleReorg** on a real transaction-set or pending-anchor hash mismatch.

Live and cached payloads stay scoped to one payload ID. Identical
retransmissions are ignored; conflicting content for the same identity fails
closed into recovery. A height that saw conflicting fragments is tombstoned
until canonical advances.

If the canonical subscription lags or delivers an empty commit, the extension
resyncs from the provider's latest recovered block instead of waiting for the
next subscription event.

## RPC Extensions

This crate provides pending-state-aware Ethereum RPC implementations used by
`base-flashblocks-node`:

- **`eth_getBlockByNumber("pending", ...)`**: returns the latest pending block built from flashblocks.
- **`eth_getTransactionReceipt`** and **`eth_getTransactionByHash`**: check canonical data first, then flashblocks pending state.
- **`eth_getBalance`**, **`eth_getTransactionCount`**, **`eth_call`**, **`eth_estimateGas`**, and **`eth_simulateV1`**: use flashblocks pending state when requested with the `pending` tag.
- **`eth_getLogs`**: combines historical logs with pending flashblock logs when the range ends at `pending`.
- **`eth_getBlockTransactionCountByNumber("pending")`**: returns the transaction count from the latest pending flashblock state.
- **`eth_sendRawTransactionSync`**: sends a raw transaction and waits for inclusion in flashblocks or the canonical chain.
- **`eth_subscribe("newFlashblocks")`**: streams pending block updates from flashblocks.
- **`eth_subscribe("pendingLogs", filter)`**: streams logs from the latest flashblock.
- **`eth_subscribe("newFlashblockTransactions", ...)`**: streams transaction hashes or full transactions from the latest flashblock.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-flashblocks = { git = "https://github.com/base/base" }
```

Subscribe to flashblocks and process state updates:

```rust,ignore
use std::{sync::Arc, time::Duration};

use base_flashblocks::{
    FlashblocksAPI, FlashblocksState, FlashblocksSubscriber, PendingBlocksAPI,
};
use url::Url;

let flashblocks_url = Url::parse("ws://127.0.0.1:1111")?;
let state = Arc::new(FlashblocksState::new(3));

// Start the state processor after a node provider is available.
state.start(provider.clone());

// Connect to the builder's flashblocks WebSocket and forward decoded payloads into state.
let mut subscriber =
    FlashblocksSubscriber::new(Arc::clone(&state), flashblocks_url, Duration::from_secs(30));
subscriber.start();

// Read the current pending snapshot.
let pending_blocks = state.get_pending_blocks();
let pending_block = pending_blocks.get_block(true);

// Subscribe to future pending snapshot updates.
let mut updates = state.subscribe_to_flashblocks();
while let Ok(pending) = updates.recv().await {
    let block = pending.get_latest_block(true);
    println!("pending block: {}", block.header.number);
}
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
