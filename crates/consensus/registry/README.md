# `base-consensus-registry`

<a href="https://crates.io/crates/base-consensus-registry"><img src="https://img.shields.io/crates/v/base-consensus-registry.svg?label=base-consensus-registry&labelColor=2a2f35" alt="base-consensus-registry"></a>
<a href="https://specs.base.org"><img src="https://img.shields.io/badge/Docs-854a15?style=flat&labelColor=1C2C2E&color=BEC5C9&logo=mdBook&logoColor=BEC5C9" alt="Docs" /></a>

## Overview

`base-consensus-registry` exports L1 chain configurations for known Ethereum networks
(Mainnet, Sepolia, Holesky, Hoodi) as `alloy_genesis::ChainConfig` instances. `no_std`
compatible when default features are disabled.

Base L2 rollup configs live in [`base-common-chains`][base-common-chains], derived from the
canonical [`ChainConfig`][chain-config] table.

## Usage

```rust
use base_consensus_registry::l1_config;

let l1_chain_id = 1; // Ethereum mainnet
let cfg = l1_config(l1_chain_id);
```

[base-common-chains]: ../../common/chains
[chain-config]: ../../common/chains/src/config.rs

## Feature Flags

- `std`: Uses the standard library to pull in environment variables.

## Credits

[alloy] and [op-alloy] for creating and maintaining high quality Ethereum and Base types in rust.


<!-- Hyperlinks -->

[serde]: https://crates.io/crates/serde
[toml]: https://crates.io/crates/toml
[alloy]: https://github.com/alloy-rs/alloy
[op-alloy]: https://github.com/alloy-rs/op-alloy

[sc]: https://crates.io/crates/base-consensus-registry

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
