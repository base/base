# `base-cli-utils`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

CLI utilities for the Base Reth node.

## Overview

- **`Version`**: Client versioning and P2P identification.
- **`GlobalArgs`**: Common CLI arguments (chain ID, logging).
- **`LoggingArgs`**: Verbosity levels, output formats, file rotation.
- **`runtime`**: Tokio runtime with Ctrl+C shutdown handling.

## Usage

```toml
[dependencies]
base-cli-utils = { git = "https://github.com/base/base" }
```

```rust,ignore
use base_cli_utils::{GlobalArgs, RuntimeManager, Version};
use clap::Parser;

#[derive(Parser)]
struct MyCli {
    #[command(flatten)]
    global: GlobalArgs,
}

fn main() -> eyre::Result<()> {
    Version::init();
    let _cli = MyCli::parse();

    RuntimeManager::run_until_ctrl_c(async {
        // ... your async code ...
        Ok(())
    })
}
```

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
