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

## Usage

Enable the endpoint with `--enable-profiling` (env `ENABLE_PROFILING`). It listens on
`--profiling.port` (default `6061`). `--profiling.max-seconds` (default `60`) bounds the
capture window and `--profiling.default-frequency` (default `101` Hz) sets the sampling rate
used when a request omits one.

Capture a profile with a GET request. `seconds` must be between `1` and `--profiling.max-seconds`;
`frequency` must be between `1` and `1000` Hz:

```bash
curl -o profile.pb.gz "http://localhost:6061/debug/pprof/profile?seconds=30&frequency=101"
go tool pprof profile.pb.gz
```

Do not enable this on a block-producing node.

## Symbols

The `release` and `maxperf` build profiles set `strip = "symbols"`, so profiles captured
from those binaries resolve to raw addresses instead of function names. To get symbolized
output, either build the node with a symbol-preserving profile or symbolize the capture
afterwards against a matching unstripped binary:

- `cargo build --profile profiling` keeps line-table debug info, disables LTO, and (in Docker
  builds, via `docker-bake.hcl`) forces frame pointers for accurate stacks. This is the
  intended profile for on-node captures.
- `cargo build --profile release-symbols` keeps `release` codegen but restores symbols, for
  when release-identical performance matters more than unwind accuracy.
- Debug info in these profiles is not loaded at runtime, so it costs binary size only, not
  RSS or CPU.
