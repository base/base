## `kona-registry`

<a href="https://github.com/op-rs/kona/actions/workflows/rust_ci.yaml"><img src="https://github.com/op-rs/kona/actions/workflows/rust_ci.yaml/badge.svg?label=ci" alt="CI"></a>
<a href="https://crates.io/crates/kona-registry"><img src="https://img.shields.io/crates/v/kona-registry.svg?label=kona-registry&labelColor=2a2f35" alt="kona-registry"></a>
<a href="https://github.com/op-rs/kona/blob/main/LICENSE.md"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>
<a href="https://rollup.yoga"><img src="https://img.shields.io/badge/Docs-854a15?style=flat&labelColor=1C2C2E&color=BEC5C9&logo=mdBook&logoColor=BEC5C9" alt="Docs" /></a>

[`kona-registry`][sc] is a `no_std` crate that exports chain configurations for Base networks.
Chain configurations are stored as TOML files in the `configs/` directory, parsed at init time
via [`serde`][serde] and the [`toml`][toml] crate.

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
use kona_registry::Registry;

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

[sc]: https://crates.io/crates/kona-registry
