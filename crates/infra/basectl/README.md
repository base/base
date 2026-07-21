# `basectl-cli`

TUI-based CLI tool for Base infrastructure monitoring.

## Overview

Provides an interactive terminal UI for monitoring Base infrastructure: block production rates,
node sync status, flashblock throughput, and system metrics. `run_app` launches the full TUI
with configurable views. Also supports `run_flashblocks_json` for non-interactive JSON output,
suitable for piping into other tools.

## Pods View

`basectl monitor pods` displays Kubernetes pod status from groups defined in a
local network config. Keep environment-specific names, namespaces, contexts, and
URLs in user-local config; this public crate only stores the generic schema.

```yaml
pods:
  refresh_interval_ms: 1000
  groups:
    - alias: example
      label: Example
      context: example-context
      namespace: example-namespace
```

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
