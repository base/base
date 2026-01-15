# `base-builder-cli`

CLI argument types for the Base block builder.

## Overview

This crate provides reusable CLI argument types for configuring the OP builder:

- **`OpRbuilderArgs`**: Main builder configuration including rollup, flashblocks, and telemetry settings
- **`FlashblocksArgs`**: Flashblocks-specific configuration (ports, timing, state root)
- **`TelemetryArgs`**: OpenTelemetry configuration for tracing

## Usage

```toml
[dependencies]
base-builder-cli = { workspace = true }
```

```rust
use base_builder_cli::{OpRbuilderArgs, FlashblocksArgs, TelemetryArgs};
use clap::Parser;

#[derive(Parser)]
struct Cli {
    #[clap(flatten)]
    builder_args: OpRbuilderArgs,
}
```

## License

Licensed under the MIT License.
