## `base-consensus-registry`

<a href="https://crates.io/crates/base-consensus-registry"><img src="https://img.shields.io/crates/v/base-consensus-registry.svg?label=base-consensus-registry&labelColor=2a2f35" alt="base-consensus-registry"></a>
<a href="https://rollup.yoga"><img src="https://img.shields.io/badge/Docs-854a15?style=flat&labelColor=1C2C2E&color=BEC5C9&logo=mdBook&logoColor=BEC5C9" alt="Docs" /></a>

[`base-consensus-registry`][sc] is a `no_std` crate that exports chain configurations for Base networks.
Chain configurations are stored as TOML files in the `configs/` directory, parsed at init time
via [`serde`][serde] and the [`toml`][toml] crate.

### Usage

Add the following to your `Cargo.toml`.

```toml
[dependencies]
base-consensus-registry = "0.1.0"
```

To make `base-consensus-registry` `no_std`, toggle `default-features` off like so.

```toml
[dependencies]
base-consensus-registry = { version = "0.1.0", default-features = false }
```

Below demonstrates getting the `RollupConfig` for Base Mainnet (Chain ID `8453`).

```rust
use base_consensus_registry::Registry;

let base_chain_id = 8453;
let base_rollup_config = Registry::rollup_config(base_chain_id);
println!("Base Mainnet Rollup Config: {:?}", base_rollup_config);
```


### Feature Flags

- `std`: Uses the standard library to pull in environment variables.


### Credits

[alloy] and [op-alloy] for creating and maintaining high quality Ethereum and Optimism types in rust.


<!-- Hyperlinks -->

[serde]: https://crates.io/crates/serde
[toml]: https://crates.io/crates/toml
[alloy]: https://github.com/alloy-rs/alloy
[op-alloy]: https://github.com/alloy-rs/op-alloy

[sc]: https://crates.io/crates/base-consensus-registry
