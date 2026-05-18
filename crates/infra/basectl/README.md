# `basectl-cli`

TUI-based CLI tool for Base infrastructure monitoring.

## Overview

Provides an interactive terminal UI for monitoring Base infrastructure: block production rates,
node sync status, flashblock throughput, and system metrics. `run_app` launches the full TUI
with configurable views. Also supports `run_flashblocks_json` for non-interactive JSON output,
suitable for piping into other tools.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
basectl-cli = { workspace = true }
```

```rust,ignore
use basectl_cli::{run_app, MonitoringConfig};

let config = MonitoringConfig::from_cli(args);
run_app(config).await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
