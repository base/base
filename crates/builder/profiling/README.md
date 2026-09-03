# `base-builder-profiling`

Continuous CPU profiling for the shadow builder.

Runs a sampling profiler in-process and exposes the captured profiles over a small HTTP
server, so a running builder can be sampled on demand without restarting it or attaching an
external profiler. Profiles are rendered either as an SVG flamegraph or as a gzipped
`pprof` protobuf (`.pb.gz`) that standard `pprof` tooling can read directly.

The profiler is wired into the builder as a node extension, which owns the profiler's
lifetime and binds the HTTP server alongside the builder's other observability endpoints.
