# `base-consensus-genesis`

<a href="https://crates.io/crates/base-consensus-genesis"><img src="https://img.shields.io/crates/v/base-consensus-genesis.svg" alt="base-consensus-genesis crate"></a>
<a href="https://rollup.yoga"><img src="https://img.shields.io/badge/Docs-854a15?style=flat&labelColor=1C2C2E&color=BEC5C9&logo=mdBook&logoColor=BEC5C9" alt="Docs" /></a>

## Overview

Genesis types for Optimism. Provides the `RollupConfig` type — the primary configuration
for OP Stack chains — encoding hardfork activation timestamps, L1 and L2 genesis block
information, batch inbox address, and system config. `no_std` compatible when default
features are disabled.

## Usage

_By default, `base-consensus-genesis` enables both `std` and `serde` features._

If you're working in a `no_std` environment, disable default features like so.

```toml
[dependencies]
base-consensus-genesis = { version = "x.y.z", default-features = false, features = ["serde"] }
```

### Rollup Config

`base-consensus-genesis` exports a `RollupConfig`, the primary genesis type for Optimism Consensus.


<!-- Links -->

[alloy-genesis]: https://github.com/alloy-rs

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
