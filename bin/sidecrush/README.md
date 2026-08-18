# `base-sidecrush-bin`

Block-production health-check sidecar binary for Base.

Parses CLI arguments and delegates to the `base-sidecrush` library, which polls
an execution-layer node's HTTP RPC endpoint and reports on latest-block age.
