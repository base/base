# base-execution-b20-prefetch

Background worker pool that resolves B20 precompile storage-prefetch hints
against the node's live state provider.

B20 operations know the exact storage slots they will read before executing
(they are derivable from calldata alone), but the journaled EVM read path
resolves them one at a time — on a state database larger than the page cache,
each cold read costs hundreds of microseconds of serial page faults. This pool
receives hint batches from precompile dispatch (via
`base_precompile_storage::PrefetchHint`) and fans the reads out across worker
threads holding independent state-provider handles, so the pages fault in
concurrently instead of serially.

Prefetching is purely a page-cache warmer: fetched values are discarded and
the metered journaled reads that follow are unchanged, so enabling or
disabling it has no consensus-visible effect. It is disabled unless the node
is started with a non-zero `--b20.prefetch-workers`.

Per-read wall time is recorded in the `b20.prefetch.read_seconds` histogram,
alongside enqueue/drop/error counters, so the real-world latency distribution
of these reads can be measured directly from a running fleet.
