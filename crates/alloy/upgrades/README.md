# `base-alloy-hardforks`

Named bindings for hardforks.

## Overview

Defines the `OpHardfork` enum and `OpHardforks` trait for the Base hardfork sequence
(Bedrock, Canyon, Delta, Ecotone, Fjord, Granite, Holocene, Isthmus, Jovian). Provides concrete
activation timestamps for Base Mainnet, Base Sepolia, and devnet chains as typed constants, and
`OpChainHardforks` for per-chain configuration.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-alloy-hardforks = { workspace = true }
```

```rust,ignore
use base_alloy_hardforks::{OpHardfork, BASE_MAINNET_BEDROCK_BLOCK};

let fork = OpHardfork::Jovian;
println!("Jovian name: {}", fork.name());
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
