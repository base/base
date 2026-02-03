# `base-builder`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Block builder library for Base. Provides flashblocks payload building infrastructure, which progressively builds block chunks at sub-second intervals.

## Overview

- **`flashblocks`**: Progressive block builder that produces block chunks at short intervals, publishing them via WebSocket before merging into full blocks.
- **`launcher`**: Node launcher utilities for starting the builder.
- **`tx_data_store`**: Transaction data storage and retrieval service with RPC extensions.

## Features

- `jemalloc`: Use jemalloc allocator (default).
- `jemalloc-prof`: Enable jemalloc profiling.
- `testing`: Enable testing utilities and framework.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-builder = { git = "https://github.com/base/base" }
```

To run the builder, use the [`base-builder`](../../../bin/builder/) binary.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
