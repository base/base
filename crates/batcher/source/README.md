# base-batcher-source

L2 unsafe block source for the Base batcher.

Provides the `UnsafeBlockSource` trait and `HybridBlockSource` implementation
that combines WebSocket subscription and HTTP polling with built-in
deduplication and reorg detection.

## Components

- **`UnsafeBlockSource`** — async trait for streaming L2 block events
- **`L2BlockEvent`** — new block or reorg signal
- **`HybridBlockSource`** — races a subscription stream against an interval-based poller
- **`PollingSource`** — trait for fetching the current unsafe head block
- **`InMemoryBlockSource`** (`test_utils`) — in-memory source for action tests
