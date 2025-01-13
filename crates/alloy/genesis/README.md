## `op-alloy-genesis`

<a href="https://github.com/alloy-rs/op-alloy/actions/workflows/ci.yml"><img src="https://github.com/alloy-rs/op-alloy/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://crates.io/crates/op-alloy-genesis"><img src="https://img.shields.io/crates/v/op-alloy-genesis.svg" alt="op-alloy-genesis crate"></a>
<a href="https://github.com/alloy-rs/op-alloy/blob/main/LICENSE-MIT"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>
<a href="https://github.com/alloy-rs/op-alloy/blob/main/LICENSE-APACHE"><img src="https://img.shields.io/badge/License-APACHE-d1d1f6.svg?label=license&labelColor=2a2f35" alt="Apache License"></a>
<a href="https://alloy-rs.github.io/op-alloy"><img src="https://img.shields.io/badge/Book-854a15?logo=mdBook&labelColor=2a2f35" alt="Book"></a>


Genesis types for Optimism.

### Usage

_By default, `op-alloy-genesis` enables both `std` and `serde` features._

If you're working in a `no_std` environment (like [`kona`][kona]), disable default features like so.

```toml
[dependencies]
op-alloy-genesis = { version = "x.y.z", default-features = false, features = ["serde"] }
```


#### Rollup Config

`op-alloy-genesis` exports a `RollupConfig`, the primary genesis type for Optimism Consensus.


### Provenance

This is based off of [alloy-genesis].


<!-- Links -->

[alloy-genesis]: https://github.com/alloy-rs
[kona]: https://github.com/op-rs/kona/blob/main/Cargo.toml#L137
