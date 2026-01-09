# `base-cli-utils`

<a href="https://github.com/base/node-reth/actions/workflows/ci.yml"><img src="https://github.com/base/node-reth/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/node-reth/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

CLI utilities for the Base Reth node.

## Overview

- **`Version`**: Client versioning and P2P identification.
- **`GlobalArgs`**: Common CLI arguments (chain ID, logging).
- **`LoggingArgs`**: Verbosity levels, output formats, file rotation.
- **`runtime`**: Tokio runtime with Ctrl+C shutdown handling.

## Usage

```toml
[dependencies]
base-cli-utils = { git = "https://github.com/base/node-reth" }
```

```rust,ignore
use base_cli_utils::{GlobalArgs, Version};
use base_cli_utils::runtime::{build_runtime, run_until_ctrl_c};
use clap::Parser;

#[derive(Parser)]
struct MyCli {
    #[command(flatten)]
    global: GlobalArgs,
}

fn main() -> eyre::Result<()> {
    Version::init();
    let cli = MyCli::parse();

    let runtime = build_runtime()?;
    runtime.block_on(run_until_ctrl_c(async { /* ... */ }))
}
```

## License

[MIT License](https://github.com/base/node-reth/blob/main/LICENSE)
