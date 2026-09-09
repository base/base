# `base-builder-core`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Block builder library for Base. `BuilderConfig::into_payload_service_config` configures the full-block payload
service with the real transaction pool, DA and gas limits, and validity predicates.
Core node startup registers transaction insertion and shadow validity RPCs from `BuilderApiConfig`.

## Features

- `jemalloc`: Use jemalloc allocator (default).
- `jemalloc-prof`: Enable jemalloc profiling.
- `test-utils`: Enable test node and transaction utilities.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-builder-core = { git = "https://github.com/base/base" }
```

To run the builder, use the [`base sequencer`](../../../bin/base/) command.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
