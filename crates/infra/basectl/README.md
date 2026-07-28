# `basectl-cli`

CLI parser, command implementations, and interactive monitor for Base infrastructure.

## Overview

Owns the `basectl` clap parser and all command behavior, including block, sync,
txpool, peer, proof, conductor, sequencer, and diagnostic workflows. `Cli::run`
dispatches parsed commands and returns a process outcome.

The crate also provides the interactive terminal monitor for block production,
node sync status, flashblock throughput, and system metrics.

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
use basectl_cli::Cli;
use clap::Parser;

let outcome = Cli::parse().run().await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
