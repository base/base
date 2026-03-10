# `base-alloy-upgrades`

Named bindings for network upgrades.

## Overview

Defines the `BaseUpgrade` enum and `BaseUpgrades` trait for the Base upgrade sequence
(Bedrock, Canyon, Ecotone, Fjord, Granite, Holocene, Isthmus, Jovian, BaseV1). Provides concrete
activation timestamps for Base Mainnet, Base Sepolia, and devnet chains as typed constants, and
`BaseChainUpgrades` for per-chain configuration.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-alloy-upgrades = { workspace = true }
```

```rust,ignore
use base_alloy_upgrades::{BaseUpgrade, BASE_MAINNET_BEDROCK_BLOCK};

let fork = BaseUpgrade::Jovian;
println!("Jovian name: {}", fork.name());
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
