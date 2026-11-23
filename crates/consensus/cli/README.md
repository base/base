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

## Historical execution startup

A stopped regular sequencer can opt into historical execution-layer synchronization with
`--sequencer.stopped --sequencer.historical-el-sync-target '<L2BlockInfo JSON>'`. Supply the
complete `unsafe_l2` object from a trusted node's `optimism_syncStatus` response, including
its L1 origin and sequence number. This is an operator-authenticated target, not automatic
discovery or validation of an arbitrary RPC endpoint.

The option is disabled by default and cannot be combined with isolated, shadow, or recovery
sequencing. A configured conductor must report non-leadership. Startup blocks local builds
and admin activation until the target is execution-valid and the label/reset handoff succeeds;
it then resumes normal derivation while leaving sequencing stopped. Start sequencing through
the normal admin flow only after checking the resulting heads. A failed or timed-out startup
does not release the gate.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
