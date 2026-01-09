# `base-cli-utils`

<a href="https://github.com/base/node-reth/actions/workflows/ci.yml"><img src="https://github.com/base/node-reth/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/node-reth/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

CLI utilities for the Base Reth node. Provides shared infrastructure for versioning, argument parsing, logging configuration, and runtime management.

## Overview

- **`Version`**: Handles versioning metadata for the Base Reth node, including client version strings and P2P identification.
- **`GlobalArgs`**: Common CLI arguments with network chain ID and logging configuration.
- **`LoggingArgs`**: Structured logging configuration with verbosity levels, output formats, and file rotation.
- **`runtime`**: Tokio runtime utilities with graceful Ctrl+C shutdown handling.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-cli-utils = { git = "https://github.com/base/node-reth" }
```

### Version Initialization

Initialize versioning at node startup:

```rust,ignore
use base_cli_utils::Version;

// Initialize version metadata before starting the node
Version::init();

// Access the client version string
let client_version = Version::NODE_RETH_CLIENT_VERSION;
```

### Global Arguments

Use `GlobalArgs` for common CLI configuration:

```rust,ignore
use base_cli_utils::GlobalArgs;
use clap::Parser;

#[derive(Parser)]
struct MyCli {
    #[command(flatten)]
    global: GlobalArgs,
}

let cli = MyCli::parse();
println!("Chain ID: {}", cli.global.chain_id);
println!("Log level: {:?}", cli.global.logging.log_level());
```

Command line usage:

```bash
# Default (Base Mainnet, WARN level)
my-app

# Base Sepolia with INFO logging
my-app --chain-id 84532 -v

# Using environment variable
BASE_NETWORK=84532 my-app -vv
```

### Logging Configuration

Use `LoggingArgs` for standalone logging configuration:

```rust,ignore
use base_cli_utils::LoggingArgs;
use clap::Parser;

#[derive(Parser)]
struct MyCli {
    #[command(flatten)]
    logging: LoggingArgs,
}

let cli = MyCli::parse();
let level = cli.logging.log_level();
let filter = cli.logging.log_level_filter();
```

Verbosity levels:
- No flag: `WARN` level
- `-v`: `INFO` level
- `-vv`: `DEBUG` level
- `-vvv`: `TRACE` level

Output formats:
- `--log-format full`: Complete format with timestamp, level, target, and spans
- `--log-format compact`: Minimal format with just level and message
- `--log-format json`: Structured JSON format for log aggregation

File logging with rotation:

```bash
my-app -vv --log-file /var/log/app.log --log-rotation daily
```

### Runtime Utilities

Build and run a Tokio runtime with graceful shutdown:

```rust,ignore
use base_cli_utils::runtime::{build_runtime, run_until_ctrl_c};

async fn my_long_running_task() {
    loop {
        // Do some work
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

fn main() -> eyre::Result<()> {
    let runtime = build_runtime()?;
    runtime.block_on(async {
        run_until_ctrl_c(my_long_running_task()).await
    })
}
```

For fallible tasks:

```rust,ignore
use base_cli_utils::runtime::{build_runtime, run_until_ctrl_c_fallible};

async fn my_fallible_task() -> eyre::Result<()> {
    // Do some work that might fail
    Ok(())
}

fn main() -> eyre::Result<()> {
    let runtime = build_runtime()?;
    runtime.block_on(async {
        run_until_ctrl_c_fallible(my_fallible_task()).await
    })
}
```

## License

Licensed under the [MIT License](https://github.com/base/node-reth/blob/main/LICENSE).
