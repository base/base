# `base-sidecrush-bin`

Block-production health-check sidecar binary for Base.

Parses CLI arguments and delegates to the `base-sidecrush` library, which polls
an execution-layer node's HTTP RPC endpoint and reports on latest-block age.

Set `BBHC_SIDECAR_WS_URL` (for example `ws://localhost:8546`) to measure block
age as the time since the last `newHeads` notification instead. Use this for
sub-second block times, where whole-second header timestamps are too coarse.
