# `base-builder-core`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Block builder library for Base. Wires the native Base payload builder from `base-execution-payload-builder` into a Base node together with the builder RPC extensions and the shared metering store, and provides the in-process test harness used by builder integration tests.

## Overview

- **`NativePayloadServiceBuilder`**: Spawns the native payload builder behind reth's basic payload job generator, building one full block per payload job.
- **`BuilderConfig`**: Builder settings shared between the payload service and the RPC extensions, including DA and gas limits, validity-predicate tuning, resource metering, and the rejection cache.
- **`BuilderApiExtension`**: Registers the `base_insertValidatedTransaction` RPC endpoint.
- **`test_utils`**: Local node instances, chain drivers, transaction builders, and pool observers for integration tests (behind the `test-utils` feature).

## Features

- `jemalloc`: Use jemalloc allocator.
- `jemalloc-prof`: Enable jemalloc profiling.
- `metrics`: Export builder metrics (default).
- `test-utils`: Enable the in-process test harness.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-builder-core = { git = "https://github.com/base/base" }
```

To run the builder, use the [`base-builder`](../../../bin/builder/) binary.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
