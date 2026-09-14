# `base-execution-profiling`

Opt-in CPU profiling for a running Base node.

Runs a sampling profiler in-process and exposes captured profiles over a small HTTP server,
so a running node can be sampled on demand without restarting it or attaching an external
profiler. Profiles are rendered as a gzipped `pprof` protobuf (`.pb.gz`) that standard
`pprof` tooling can read directly.

The profiler is wired in as a node extension, which owns the profiler's lifetime and binds
the HTTP server alongside the node's other observability endpoints. It installs on any node
built via `base-node-runner`, not just the builder, and is disabled by default behind
`--enable-profiling`.
