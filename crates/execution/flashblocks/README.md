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

## Pending State

`StateProcessor` merges two inputs into a single pending snapshot: the flashblock stream from the
builder and the node's canonical block notifications. Both arrive as `StateUpdate` values on one
unbounded queue and are applied in order, so the processor's view of the chain falls behind the
node's real tip whenever applying updates is slower than receiving them.

### The invariant

A published snapshot either tracks flashblocks anchored near the node's current canonical tip, or
there is no snapshot at all.

This matters because consumers execute against `PendingBlocks::canonical_block_number`, the
canonical block the overlay is layered on. While that block stays close to the tip, callers read
the node's in-memory canonical state. An overlay stranded on an old anchor forces them onto
historical state instead, which is slow enough to starve the processor that produced the overlay
and keep it stranded.

`max_pending_blocks_depth` (CLI `--max-pending-blocks-depth`, default 3) sets the bound. It is
measured from the earliest pending block, matching the depth `CanonicalBlockReconciler` already
uses, so the reconciler never builds a snapshot that the tip checks then discard. The anchor is
the block below the earliest pending block, so it may sit up to `max_pending_blocks_depth + 1`
blocks behind the tip.

Bounding the distance rather than requiring the anchor to equal the tip is deliberate. When
flashblocks for the next block arrive before the processor has applied the current canonical
block, pending legitimately spans blocks at or below the tip while still extending past it, and
that state is correct and useful. A small window keeps it alive and absorbs the delay between a
canonical notification and the block becoming visible through the provider, so the processor does
not need to track which updates raced with which.

### Enforcement

Guards run in two places, and that split is what bounds staleness.

**On the receiving tasks.** `on_canonical_block_received` and `on_flashblock_received` run on the
subscription tasks rather than on the processor, so they keep working while the processor is inside
a single expensive update. They use the notified block height directly instead of reading the
provider.

- A canonical notification records the new height and immediately drops the published snapshot if
  it is anchored more than `max_pending_blocks_depth` behind it. Because this runs at chain speed,
  a snapshot cannot stay readable through a long apply. The drop is a compare-and-swap against the
  snapshot that was judged, so a snapshot the processor published concurrently, which is
  necessarily anchored on a later tip, is left alone.
- A flashblock for a block at or below the last notified canonical height is dropped before it is
  queued, so the queue never accumulates work that could not produce a publishable snapshot.

The recorded height is the one most recently notified rather than the highest ever seen, so a reorg
that lowers the tip does not suppress flashblocks built on the replacement chain.

**In the processor.** These read the tip from the provider rather than trusting the height of the
queued update, because a lagging queue reports a stale height and makes a guard evaluate against a
chain position the node left long ago.

- Before an update is dispatched, a snapshot anchored more than `max_pending_blocks_depth` blocks
  behind the tip is dropped. This covers advances the notification path did not report, such as the
  gap between a canonical notification and the block becoming visible through the provider.
- Flashblocks whose block the node has already canonicalized are skipped before execution. This
  catches payloads that were fresh when queued and went stale while waiting, and cached payloads
  replayed after a canonical block arrives.
- Every build path publishes through `publish_pending_blocks`, which re-reads the tip and refuses
  to publish a snapshot that is anchored too far back or no longer extends past the tip. Because it
  is the single funnel, this holds for the reorg and depth-limit rebuilds as well as for ordinary
  sequential appends.

Recovery needs no separate mechanism. Once pending is absent, the next index-0 flashblock rooted at
the current tip rebuilds a snapshot through the ordinary build path.

### Limits

Freshness is bounded by canonical notification delivery rather than by processor progress, but it
is still not evaluated at the moment a consumer reads. A consumer that cannot tolerate a snapshot
going stale between the last notification and its own read must compare its view of the tip against
`canonical_block_number` and `parent_hash` itself.

The height comparison does not detect that the anchor block was reorged out, because the
replacement sits at the same height. That is caught when `ReorgDetector` sees the replaced block's
transactions, or by a consumer comparing `parent_hash`. Detecting it here would mean checking the
anchor's hash against canonical history, which is a statement about whether a payload's declared
parent is real and belongs in payload validation.

The update queue is unbounded. Dropping superseded flashblocks before they are queued removes the
backlog shape that let lag compound, but a sustained burst of payloads that are all ahead of the
tip still grows memory. A hard bound on the queue is separate work.

### Canonical reconciliation

When a canonical block is applied, `ReorgDetector` compares the transactions pending had tracked
for that block against the block's actual transactions. If pending exists,
`CanonicalBlockReconciler` then chooses one of:

- **`CatchUp`**: canonical reached or passed pending's latest block. Pending is cleared.
- **`HandleReorg`**: the transaction sets differ. Flashblocks beyond the canonical block are
  re-executed from canonical state without reusing the existing pending state.
- **`DepthLimitExceeded`**: pending retains more than `max_pending_blocks_depth` blocks that the
  canonical chain has already covered. Pending is rebuilt from the canonical block forward.
- **`Continue`**: no conflict. The existing pending state is extended.

An empty tracked set means pending held no flashblocks for that block rather than that the block
was empty, since every L2 block carries an L1 attributes deposit. That case is not a reorg, and
treating it as one would rebuild the whole snapshot for a block that needs no work.

Reconciliation compares against the greater of the notified block height and the provider tip.
Using only the notified height leaves `CatchUp` and `DepthLimitExceeded` unable to fire while
pending drifts away from the tip, because both thresholds move with the stale queue.

Flashblocks that arrive before their parent canonical block is visible are buffered in a bounded
cache and replayed once it lands.

### Observability

- `pending_drop_stale`: snapshots dropped for being anchored too far behind the tip.
- `flashblock_superseded`: flashblocks skipped because their block was already canonical.
- `pending_clear_catchup`, `pending_clear_reorg`: clears by reconciliation outcome.
- `pending_snapshot_height`, `pending_snapshot_fb_index`: current snapshot position.

Sustained `pending_drop_stale` means the processor cannot keep up with its queue. Pending is
correct in that state, since it is absent rather than stale, but flashblock-backed RPC responses
fall back to canonical data.

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
