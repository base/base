# `base-common-chain-config`

Single source of truth for Base chain configuration and network upgrade bindings.

## Overview

Defines `BaseChainConfig` — a compile-time struct containing all chain parameters (chain IDs,
upgrade timestamps, genesis data, base fee params, contract addresses, and embedded genesis JSON).
Const chain configuration instances eliminate duplicated configuration across the workspace.

Also provides the `BaseUpgrade` enum, `BaseUpgrades` trait, and `BaseChainUpgrades` for the
Base upgrade sequence (Bedrock, Canyon, Ecotone, Fjord, Granite, Holocene, Isthmus, Jovian, Azul).

## Usage

```toml
[dependencies]
base-common-chain-config = { workspace = true }
```

```rust,ignore
use base_common_chain_config::{BaseChainConfig, BASE_MAINNET};

assert_eq!(BASE_MAINNET.chain_id, 8453);
assert_eq!(BASE_MAINNET.canyon_timestamp, 1_704_992_401);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).

# `base-common-chain-config`

<a href="https://crates.io/crates/base-common-chain-config"><img src="https://img.shields.io/crates/v/base-common-chain-config.svg" alt="base-common-chain-config crate"></a>
<a href="https://specs.base.org"><img src="https://img.shields.io/badge/Docs-854a15?style=flat&labelColor=1C2C2E&color=BEC5C9&logo=mdBook&logoColor=BEC5C9" alt="Docs" /></a>

## Overview

Genesis types for Base. Provides the `RollupConfig` type — the primary configuration
for Base chains — encoding upgrade activation timestamps, L1 and L2 genesis block
information, batch inbox address, and system config. `no_std` compatible when default
features are disabled.

## Usage

_By default, `base-common-chain-config` enables both `std` and `serde` features._

If you're working in a `no_std` environment, disable default features like so.

```toml
[dependencies]
base-common-chain-config = { version = "x.y.z", default-features = false, features = ["serde"] }
```

### Rollup Config

`base-common-chain-config` exports a `RollupConfig`, the primary genesis type for Base Consensus.


<!-- Links -->

[alloy-genesis]: https://github.com/alloy-rs

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
