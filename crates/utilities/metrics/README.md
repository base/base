# `base-metrics`

Utility macros and types for recording metrics in base crates.

## Overview

Provides declarative macros and RAII types used across the Base codebase for consistent
instrumentation. Includes `define_metrics!` for registering Prometheus metrics in a
standardized way, `timed!` for automatic duration recording, and `inflight!` for tracking
in-flight operations.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-metrics = { workspace = true }
```

```rust,ignore
use base_metrics::define_metrics;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
