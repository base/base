# base-batcher-source

L2 unsafe block source for the Base batcher.

Provides ordered L2 block polling, reorg detection, and L1 head tracking.

## Components

- **`UnsafeBlockSource`** — async trait for streaming L2 block events
- **`L2BlockEvent`** — new block or reorg signal
- **`PollingBlockSource`** — fetches consecutive L2 blocks above a safe head
- **`PollingSource`** — trait for fetching an L2 block by number
- **`L1HeadSource`** — async trait for streaming L1 head block numbers
- **`L1HeadPolling`** — trait for fetching the latest L1 head block number
- **`HybridL1HeadSource`** — combines L1 subscription and polling
- **`SourceError`** — errors of the polling adapters, which the sources retry
- **`test_utils`** (feature `test-utils`) — channel-backed sources for tests
