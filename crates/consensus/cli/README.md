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

## Execution-layer sync anchor

`--el-sync-safe-offset` (`BASE_NODE_EL_SYNC_SAFE_OFFSET`) selects the derivation
anchor age in seconds after an observed execution-layer `SYNCING` → `VALID` transition.
The default is 43,200 seconds. Anchor age follows the configured block-time schedule,
including Denim, rather than querying historical block timestamps. Zero trusts the
execution-valid synced tip as safe; it does not establish L1 finality.

A canonical SafeDB tip takes precedence over the offset. A non-canonical SafeDB tip
or L1 inclusion key is ignored for anchor selection without trimming or clearing
the database. Higher existing labels are retained. Immediate-`VALID` snapshot startup
is unchanged, and finalized only advances later through derivation.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
