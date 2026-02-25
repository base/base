# `base-cli-utils`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

CLI utilities for the Base Reth node.

## Overview

- **`parse_cli!`**: Parses CLI arguments with package version/description from Cargo.toml.
- **`init_reth_version!`**: Initializes Reth's global version metadata for P2P identification.
- **`register_version_metrics!`**: Registers `base_info{version="..."}` Prometheus metric.
- **`GlobalArgs`**: Common CLI arguments (chain ID, logging).
- **`LoggingArgs`**: Verbosity levels, output formats, file rotation.
- **`RuntimeManager`**: Tokio runtime with Ctrl+C shutdown handling.

## Usage

```toml
[dependencies]
base-cli-utils = { git = "https://github.com/base/base" }
```

### Example

```rust,ignore
use base_execution_cli::Cli;

fn main() {
    // Initialize Reth version metadata for P2P identification
    base_cli_utils::init_reth_version!();

    // Parse CLI with package version/description from Cargo.toml
    let cli = base_cli_utils::parse_cli!(Cli<ChainSpecParser, Args>);

    cli.run(|builder, args| async move {
        // Register version metrics after node starts
        base_cli_utils::register_version_metrics!();
        // ...
    });
}
```

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
