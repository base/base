# `base-consensus-upgrades`

<a href="https://crates.io/crates/base-consensus-upgrades"><img src="https://img.shields.io/crates/v/base-consensus-upgrades.svg" alt="base-consensus-upgrades crate"></a>
<a href="https://rollup.yoga"><img src="https://img.shields.io/badge/Docs-854a15?style=flat&labelColor=1C2C2E&color=BEC5C9&logo=mdBook&logoColor=BEC5C9" alt="Docs" /></a>

Consensus layer hardfork types for Base including network upgrade transactions.

## Overview

Provides typed hardfork abstractions for the OP Stack consensus layer. Defines the `Hardfork`
trait and a `Hardforks` registry, with concrete implementations for each upgrade (Ecotone, Fjord,
Isthmus, Jovian) including the network upgrade transactions that must be injected at hardfork
activation blocks.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-consensus-upgrades = { workspace = true }
```

```rust,ignore
use base_consensus_upgrades::{Hardfork, Hardforks};

let hardforks = Hardforks::default();
if hardforks.is_active::<Jovian>(timestamp) {
    // apply Jovian upgrade transactions
}
```

## Provenance

This code was ported from [op-alloy] as part of the `base` monorepo.

[op-alloy]: https://github.com/alloy-rs/op-alloy

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
