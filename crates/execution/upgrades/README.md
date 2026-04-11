# `base-execution-upgrades`

Chain upgrade definitions for Base.

## Overview

Provides execution-layer upgrade utilities by re-exporting and extending the base upgrade
definitions from `base-common-chains`. Exposes pre-configured upgrade activation schedules
for Base Mainnet (`BASE_MAINNET_UPGRADES`), Base Sepolia (`BASE_SEPOLIA_UPGRADES`), and local
devnets (`DEV_UPGRADES`), along with extension traits for querying activation status.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-upgrades = { workspace = true }
```

```rust,ignore
use base_execution_upgrades::BASE_MAINNET_UPGRADES;

let upgrades = BASE_MAINNET_UPGRADES.clone();
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
