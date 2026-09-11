# base-execution-state-prefetch

Background worker pool that resolves state-prefetch hints against the node's
live state provider.

Some execution paths know the storage slots an operation will read before
executing it, but the journaled EVM read path resolves those reads one at a
time. This pool receives hint batches through
`base_precompile_storage::PrefetchHint` and fans the reads out across worker
threads with independent state-provider handles, so database pages can fault
in concurrently.

Prefetching is purely a page-cache warmer: fetched values are discarded and
the metered journaled reads remain unchanged, so enabling or disabling it has
no consensus-visible effect. It is disabled unless the node starts with a
non-zero `--state.prefetch-workers` value.

The pool exports `state.prefetch.read_seconds` along with hint, enqueue, drop,
and read-error counters so operators can measure real storage latency and
backpressure on enabled nodes.
