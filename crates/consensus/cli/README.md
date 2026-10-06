# `base-consensus-cli`

CLI argument types for Base consensus clients.

## Overview

This crate provides reusable CLI argument types for configuring Base consensus clients:

- **`L1ClientArgs`**: L1 execution client RPC configuration
- **`L2ClientArgs`**: L2 engine API configuration with JWT handling
- **`RpcArgs`**: JSON-RPC server configuration
- **`SequencerArgs`**: Sequencer mode configuration

## Usage

```toml
[dependencies]
base-consensus-cli = { workspace = true }
```

```rust
use base_consensus_cli::{L1ClientArgs, L2ClientArgs};
use clap::Parser;

#[derive(Parser)]
struct Cli {
    #[clap(flatten)]
    l1_args: L1ClientArgs,
    #[clap(flatten)]
    l2_args: L2ClientArgs,
}
```

## Embedded lifecycle

`BootnodeP2PArgs::run` starts discovery, writes the local ENR, records readiness,
and consumes discovered peers until the stream closes. Callers check ports first.

`ConsensusNodeArgs::start_with_execution` starts consensus with embedded execution
overrides after upgrade-signal startup has already been applied. When either node
exits, it stops and awaits the other. The first node's error takes precedence;
otherwise the other node's result is returned. A failure to signal execution
shutdown is returned immediately. The shared lifecycle retains the execution
node handle until coordinated shutdown completes.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
