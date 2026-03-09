# `base-macros`

Utility helper macros for base crates.

## Overview

Provides procedural and declarative macros used across the Base codebase for consistent
instrumentation and boilerplate reduction. Currently includes metric-related macro utilities for
registering and recording Prometheus metrics in a standardized way.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-macros = { workspace = true }
```

```rust,ignore
use base_macros::metrics;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
