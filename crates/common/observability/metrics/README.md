# `base-common-observability-metrics`

Utility macros and types for recording metrics in base crates.

## Overview

Provides declarative macros and RAII types used across the Base codebase for consistent
instrumentation. Includes `define_metrics!` for registering Prometheus metrics in a
standardized way, with optional `struct = ...` naming for custom accessor types,
`timed!` for automatic duration recording, and `inflight!` for tracking in-flight
operations.

The crate also owns the `Metrics` derive re-export, thread resource measurements, and instrumented
channel wrappers. The `metrics` feature enables recording and metric registration; `common` adds
the asynchronous channel wrappers. With both disabled, instrumentation compiles to no-op handles
and supports `no_std`.

Run recording and channel tests with `cargo test -p base-common-observability-metrics --features common`.
The recording integration suites explicitly require `metrics`.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-common-observability-metrics = { workspace = true }
```

```rust,ignore
use base_common_observability_metrics::define_metrics;

define_metrics! {
    my.app
    #[describe("Total requests")]
    requests_total: counter,
}

define_metrics! {
    my.app,
    struct = MyMetrics,
    #[describe("Request duration")]
    request_duration: histogram,
}
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).

## Build label

`MetricsBuild::NAME` is embedded at compile time from `BASE_BUILD_NAME`, with `dev`
as the default. Both CLI and execution-node Prometheus recorders attach it as the
`build` label to every exported sample. For example:

```sh
BASE_BUILD_NAME=mdbx-baseline cargo build -p base-bin-base --release
```

Set the name when compiling, not when launching the binary. Cargo tracks changes to
this compile-time environment variable; no clean build is required. The existing
metric-specific labels and execution recorder's `reth_` prefix are preserved.
