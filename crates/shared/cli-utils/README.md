# `base-cli`

<a href="https://github.com/base/node-reth/actions/workflows/ci.yml"><img src="https://github.com/base/node-reth/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/node-reth/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

CLI utilities for the Base Reth node. Provides versioning and client identification for the Base node binary.

## Overview

- **`Version`**: Handles versioning metadata for the Base Reth node, including client version strings and P2P identification.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-cli = { git = "https://github.com/base/node-reth" }
```

Initialize versioning at node startup:

```rust,ignore
use base_cli::Version;

// Initialize version metadata before starting the node
Version::init();

// Access the client version string
let client_version = Version::NODE_RETH_CLIENT_VERSION;
```

## License

Licensed under the [MIT License](https://github.com/base/node-reth/blob/main/LICENSE).
