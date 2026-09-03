# base-execution-storage-prefetch

Background worker pool that resolves storage-prefetch hints against the
node's live state provider.

Some execution paths know the exact storage slots an operation will read
before executing it — natively implemented precompiles derive them from
calldata alone — but the journaled EVM read path resolves slots one at a
time. On a state database larger than the page cache, each cold read costs
hundreds of microseconds of serial page faults. This pool receives hint
batches (via `base_precompile_storage::PrefetchHint`) and fans the reads out
across worker threads holding independent state-provider handles, so the
pages fault in concurrently instead of serially.

The pool is producer-agnostic: any code path that can statically derive its
slot set may send hints. The B20 precompiles' `transfer`/`transferFrom` are
the first producers; other stateful precompiles or well-known contracts with
stable storage layouts (e.g. major ERC-20s or DEX pools) can be added
without changes here.

Prefetching is purely a page-cache warmer: fetched values are discarded and
the metered journaled reads that follow are unchanged, so enabling or
disabling it has no consensus-visible effect. It is disabled unless the node
is started with a non-zero `--storage.prefetch-workers`.

Per-read wall time is recorded in the `storage.prefetch.read_seconds`
histogram, alongside enqueue/drop/error counters, so the real-world latency
distribution of these reads can be measured directly from a running fleet.
