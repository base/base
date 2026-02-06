# `base-builder-cli`

CLI argument types for the Base block builder.

## Overview

This crate provides reusable CLI argument types for configuring the OP builder:

- **`BuilderArgs`**: Main builder configuration including rollup, flashblocks, and telemetry settings
- **`FlashblocksArgs`**: Flashblocks-specific configuration (ports, timing, state root)
- **`TelemetryArgs`**: `OpenTelemetry` configuration for tracing

These CLI types can be converted to `BuilderConfig` (from `base-builder-core`) using `TryFrom`:

```rust,ignore
use base_builder_cli::BuilderArgs;
use base_builder_core::BuilderConfig;

let args = BuilderArgs::default();
let config: BuilderConfig = args.try_into()?;
```

## Usage

```toml
[dependencies]
base-builder-cli = { workspace = true }
```

```rust
use base_builder_cli::{BuilderArgs, FlashblocksArgs, TelemetryArgs};
use clap::Parser;

#[derive(Parser)]
struct Cli {
    #[clap(flatten)]
    builder_args: BuilderArgs,
}
```

## License

Licensed under the MIT License.
