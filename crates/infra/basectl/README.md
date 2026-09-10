# `base-infra-basectl`

CLI parser, command implementations, and interactive monitor for Base infrastructure.

## Overview

Owns the `basectl` clap parser and all command behavior, including block, sync,
txpool, peer, proof, conductor, sequencer, and diagnostic workflows. `Cli::run`
dispatches parsed commands and returns a process outcome.

The crate also provides the interactive terminal monitor for block production,

## Cobalt Readiness

The upgrades monitor attaches `BaseTime` checks to Cobalt, using the live consensus
node's `optimism_rollupConfig` Cobalt timestamp. `CobaltChecker` checks a hash-pinned
L2 snapshot for `BaseTime` installation, update metadata and receipt, storage/getter
agreement, 200ms cadence, and millisecond RPC fields. Active-only checks start at
Cobalt; an absent or later Denim activation does not change their readiness state.
Denim remains visible in the upgrade schedule without separate `BaseTime` checks.

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
base-infra-basectl = { workspace = true }
```

```rust,ignore
use base_infra_basectl::Cli;
use clap::Parser;

if Cli::parse().run().await?.has_failures() {
    std::process::exit(1);
}
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
