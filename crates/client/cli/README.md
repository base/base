# `base-client-cli`

CLI argument types for Base node consensus clients.

## Overview

This crate provides reusable CLI argument types for configuring Base node consensus clients:

- **`L1ClientArgs`**: L1 execution client RPC configuration
- **`L2ClientArgs`**: L2 engine API configuration with JWT handling
- **`BuilderClientArgs`**: Block builder client configuration with JWT handling
- **`RpcArgs`**: JSON-RPC server configuration
- **`SequencerArgs`**: Sequencer mode configuration
- **`RollupBoostFlags`**: Rollup boost block builder configuration

## Usage

```toml
[dependencies]
base-client-cli = { workspace = true }
```

```rust
use base_client_cli::{L1ClientArgs, L2ClientArgs, BuilderClientArgs};
use clap::Parser;

#[derive(Parser)]
struct Cli {
    #[clap(flatten)]
    l1_args: L1ClientArgs,
    #[clap(flatten)]
    l2_args: L2ClientArgs,
    #[clap(flatten)]
    builder_args: BuilderClientArgs,
}
```

## License

Licensed under the MIT License.
