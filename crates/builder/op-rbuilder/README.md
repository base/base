# `op-rbuilder`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Optimism block builder for Base. Provides payload building infrastructure with support for standard block production and flashblocks, which progressively builds block chunks at sub-second intervals.

## Overview

- **`FlashblocksBuilder`**: Progressive block builder that produces block chunks at short intervals, publishing them via WebSocket before merging into full blocks.
- **`StandardBuilder`**: Traditional block builder that produces complete blocks at each chain block time.
- **`BuilderMode`**: Configuration enum to select between `Standard` and `Flashblocks` modes.
- **`RevertProtection`**: RPC extension for protecting transactions from reverts.
- **`TxDataStore`**: Transaction data storage and retrieval service.
- **`Signer`**: Transaction signing utilities for builder operations.

## Binaries

- **`op-rbuilder`**: Main builder binary with full node capabilities.
- **`tester`**: Testing utility binary (requires `testing` feature).

## Features

- `jemalloc`: Use jemalloc allocator (default).
- `testing`: Enable testing utilities and framework.
- `telemetry`: Enable `OpenTelemetry` integration.
- `interop`: Enable interoperability features.
- `custom-engine-api`: Enable custom engine API extensions.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
op-rbuilder = { git = "https://github.com/base/base" }
```

Run the builder:

```bash
# Standard mode
op-rbuilder node --builder.mode standard

# Flashblocks mode
op-rbuilder node --builder.mode flashblocks
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
