# `base-common-chains`

Single source of truth for Base chain configuration and network upgrade bindings.

## Overview

Defines `ChainConfig`, a compile-time struct containing all chain parameters (chain IDs,
upgrade timestamps, genesis data, base fee params, contract addresses, and embedded genesis JSON).
Const chain configuration instances eliminate duplicated configuration across the workspace.

Re-exports the `BaseUpgrade` enum from `base-common-genesis` and provides the
`BaseUpgradeExt` extension trait, `Upgrades` trait, and `ChainUpgrades` for the
Base upgrade sequence. See [`BaseUpgrade::EXECUTION_VARIANTS`]
([source](https://github.com/base/base/blob/main/crates/common/genesis/src/chain/upgrade.rs))
for the canonical execution fork ladder.

## Usage

```toml
[dependencies]
base-common-chains = { workspace = true }
```

```rust,ignore
use base_common_chains::ChainConfig;

assert_eq!(ChainConfig::MAINNET.chain_id, 8453);
assert_eq!(ChainConfig::MAINNET.canyon_timestamp, 1_704_992_401);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
