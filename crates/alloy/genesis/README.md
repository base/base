# op-alloy-genesis

<a href="https://crates.io/crates/op-alloy-genesis"><img src="https://img.shields.io/crates/v/op-alloy-genesis.svg" alt="op-alloy-genesis crate"></a>

Genesis types for Optimism.

## Usage

_By default, `op-alloy-genesis` enables both `std` and `serde` features._

If you're working in a `no_std` environment (like [`kona`][kona]), disable default features like so.

```toml
[dependencies]
op-alloy-genesis = { version = "x.y.z", default-features = false, features = ["serde"] }
```

[kona]: https://github.com/anton-rs/kona/blob/main/Cargo.toml#L137


### Rollup Config

`op-alloy-genesis` exports a `RollupConfig`, the primary genesis type for Optimism Consensus.

## Provenance

This is based off of [alloy-genesis].

[alloy-genesis]: https://github.com/alloy-rs
