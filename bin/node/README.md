## Base Reth Node

This is the main entry point for running a Base node with Reth,
including support for flashblocks, transaction tracing, and metering.

### MEV emitter dry-run live reserve

`MEV_EMITTER_ARB_DRYRUN_LIVE_RESERVE=1` is an explicit opt-in layered on
`MEV_EMITTER_ARB_DRYRUN=1`. With the flag off, AerodromeStable reserve slot 20/21
overlays remain unsupported to keep dry-run emission byte-equivalent to the base path.

With the flag on, dry-run reserve refresh uses an async Flashblocks broadcast worker
instead of the synchronous `CoreArbPendingFrameObserver`, so ahead-of-committed dry-run
observation disappears while live reserve is enabled.
