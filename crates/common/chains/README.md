# `base-common-chains`

Single source of truth for Base chain configuration and network upgrade bindings.

## Overview

Defines `BaseChainConfig` — a compile-time struct containing all chain parameters (chain IDs,
upgrade timestamps, genesis data, base fee params, contract addresses, and embedded genesis JSON).
Const instances `BASE_MAINNET`, `BASE_SEPOLIA`, and `BASE_DEVNET_0_SEPOLIA_DEV_0` eliminate
duplicated configuration across the workspace.

Also provides the `BaseUpgrade` enum, `BaseUpgrades` trait, and `BaseChainUpgrades` for the
Base upgrade sequence (Bedrock, Canyon, Ecotone, Fjord, Granite, Holocene, Isthmus, Jovian, Azul).

## Usage

```toml
[dependencies]
base-common-chains = { workspace = true }
```

```rust,ignore
use base_common_chains::{BaseChainConfig, BASE_MAINNET};

assert_eq!(BASE_MAINNET.chain_id, 8453);
assert_eq!(BASE_MAINNET.canyon_timestamp, 1_704_992_401);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
