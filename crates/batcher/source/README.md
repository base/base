# base-batcher-source

L2 unsafe block source for the Base batcher.

Provides the `UnsafeBlockSource` trait and `HybridBlockSource` implementation
that combines WebSocket subscription and HTTP polling with deduplication,
sequential gap recovery, and reorg detection.

## Components

- **`UnsafeBlockSource`** — async trait for streaming L2 block events
- **`L2BlockEvent`** — new block or reorg signal
- **`HybridBlockSource`** — merges live updates with ordered polling
- **`PollingSource`** — trait for fetching an L2 block by number
- **`InMemoryBlockSource`** (`test_utils`) — in-memory source for action tests
