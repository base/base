# `base-cli-utils`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

CLI utilities for Base binaries.

## Overview

- **`parse_cli!`**: Parses CLI arguments with package version/description from Cargo.toml.
- **`register_version_metrics!`**: Registers `base_info{version="...",binary="..."}` Prometheus metric.
- **`define_log_args!`**: Generates a `LogArgs` struct with env-var-prefixed logging flags.
- **`define_metrics_args!`**: Generates a `MetricsArgs` struct with env-var-prefixed Prometheus flags.
- **`RuntimeManager`**: Tokio runtime creation and OS signal handling (SIGINT + SIGTERM) with `CancellationToken`-based cooperative shutdown.

## Usage

```toml
[dependencies]
base-cli-utils = { git = "https://github.com/base/base" }
```

### Example

```rust,ignore
use base_execution_cli::Cli;

fn main() {
    let cli = base_cli_utils::parse_cli!(Cli<ChainSpecParser, Args>);

    cli.run(|builder, args| async move {
        base_cli_utils::register_version_metrics!();
        // ...
    });
}
```

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
