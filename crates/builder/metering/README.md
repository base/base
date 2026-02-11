# `base-builder-metering`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Resource metering backend for the OP Stack block builder. Provides a concrete [`MeteringProvider`](../core/) implementation backed by a concurrent cache with LRU eviction, along with JSON-RPC extensions for managing resource metering data.

## Overview

- **`MeteringStore`**: Thread-safe metering data cache using `DashMap` with bounded LRU eviction. Implements `MeteringProvider` from `base-builder-core`.
- **`MeteringStoreExt`**: JSON-RPC extension exposing `base_setMeteringInformation`, `base_setMeteringEnabled`, and `base_clearMeteringInformation` methods.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-builder-metering = { git = "https://github.com/base/base" }
```

Create a store and wire it into the builder config:

```rust,ignore
use std::sync::Arc;
use base_builder_metering::{MeteringStore, MeteringStoreExt};

let store = Arc::new(MeteringStore::new(true, 10_000));
// Pass `store.clone()` as `SharedMeteringProvider` into `BuilderConfig`
// Pass `store` into `MeteringStoreExt::new()` for the RPC extension
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
