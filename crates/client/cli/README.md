# `base-client-cli`

CLI argument types for Base node consensus clients.

## Overview

This crate provides reusable CLI argument types for configuring Base node consensus clients:

- **`L1ClientArgs`**: L1 execution client RPC configuration
- **`L2ClientArgs`**: L2 engine API configuration with JWT handling
- **`RpcArgs`**: JSON-RPC server configuration
- **`SequencerArgs`**: Sequencer mode configuration

## Usage

```toml
[dependencies]
base-client-cli = { workspace = true }
```

```rust
use base_client_cli::{L1ClientArgs, L2ClientArgs};
use clap::Parser;

#[derive(Parser)]
struct Cli {
    #[clap(flatten)]
    l1_args: L1ClientArgs,
    #[clap(flatten)]
    l2_args: L2ClientArgs,
}
```

## License

Licensed under the MIT License.
