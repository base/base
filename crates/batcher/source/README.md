# base-batcher-source

L2 unsafe block source for the Base batcher.

Provides ordered L2 block polling, reorg detection, and L1 head tracking.

## Components

- **`UnsafeBlockSource`** — async trait for streaming L2 block events
- **`L2BlockEvent`** — new block or reorg signal
- **`PollingBlockSource`** — fetches consecutive L2 blocks above a safe head
- **`PollingSource`** — trait for fetching an L2 block by number
- **`HybridL1HeadSource`** — combines L1 subscription and polling
- **`InMemoryBlockSource`** (`test_utils`) — in-memory source for action tests
