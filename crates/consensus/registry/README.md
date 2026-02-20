## `kona-registry`

<a href="https://github.com/op-rs/kona/actions/workflows/rust_ci.yaml"><img src="https://github.com/op-rs/kona/actions/workflows/rust_ci.yaml/badge.svg?label=ci" alt="CI"></a>
<a href="https://crates.io/crates/kona-registry"><img src="https://img.shields.io/crates/v/kona-registry.svg?label=kona-registry&labelColor=2a2f35" alt="kona-registry"></a>
<a href="https://github.com/op-rs/kona/blob/main/LICENSE.md"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>
<a href="https://rollup.yoga"><img src="https://img.shields.io/badge/Docs-854a15?style=flat&labelColor=1C2C2E&color=BEC5C9&logo=mdBook&logoColor=BEC5C9" alt="Docs" /></a>

[`kona-registry`][sc] is a `no_std` crate that exports rust type definitions for chains
in the [`superchain-registry`][osr]. Chain configurations are stored as TOML files in
the `configs/` directory, parsed at init time via [`serde`][serde] and the [`toml`][toml] crate.
To use the [`kona-registry`][sc] crate, add it as a dependency to a `Cargo.toml`.

```toml
kona-registry = "0.1.0"
```

[`kona-registry`][sc] declares lazy evaluated statics that expose `ChainConfig`s, `RollupConfig`s,
and `Chain` objects for all chains with static definitions in the superchain registry.

There are three core statics exposed by the [`kona-registry`][sc].
- `CHAINS`: A list of chain objects containing the superchain metadata for this chain.
- `OPCHAINS`: A map from chain id to `ChainConfig`.
- `ROLLUP_CONFIGS`: A map from chain id to `RollupConfig`.

[`kona-registry`][sc] exports the _complete_ list of chains within the superchain, as well as each
chain's `RollupConfig`s and `ChainConfig`s.

### Usage

Add the following to your `Cargo.toml`.

```toml
[dependencies]
kona-registry = "0.1.0"
```

To make `kona-registry` `no_std`, toggle `default-features` off like so.

```toml
[dependencies]
kona-registry = { version = "0.1.0", default-features = false }
```

Below demonstrates getting the `RollupConfig` for Base Mainnet (Chain ID `8453`).

```rust
use kona_registry::ROLLUP_CONFIGS;

let base_chain_id = 8453;
let base_rollup_config = ROLLUP_CONFIGS.get(&base_chain_id);
println!("Base Mainnet Rollup Config: {:?}", base_rollup_config);
```

A mapping from chain id to `ChainConfig` is also available.

```rust
use kona_registry::OPCHAINS;

let base_chain_id = 8453;
let base_chain_config = OPCHAINS.get(&base_chain_id);
println!("Base Mainnet Chain Config: {:?}", base_chain_config);
```


### Feature Flags

- `std`: Uses the standard library to pull in environment variables.


### Credits

[superchain-registry][osr] contributors for building and maintaining superchain types.

[alloy] and [op-alloy] for creating and maintaining high quality Ethereum and Optimism types in rust.


<!-- Hyperlinks -->

[serde]: https://crates.io/crates/serde
[toml]: https://crates.io/crates/toml
[alloy]: https://github.com/alloy-rs/alloy
[op-alloy]: https://github.com/alloy-rs/op-alloy
[op-superchain]: https://docs.optimism.io/stack/explainer
[osr]: https://github.com/ethereum-optimism/superchain-registry

[s]: ./configs
[sc]: https://crates.io/crates/kona-registry
[g]: https://crates.io/crates/kona-genesis

[chains]: https://docs.rs/kona-registry/latest/kona_registry/struct.CHAINS.html
[opchains]: https://docs.rs/kona-registry/latest/kona_registry/struct.OPCHAINS.html
[rollups]: https://docs.rs/kona-registry/latest/kona_registry/struct.ROLLUP_CONFIGS.html
[superchains]: https://docs.rs/kona-genesis/latest/kona_genesis/struct.Superchain.html
