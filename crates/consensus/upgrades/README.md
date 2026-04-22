# `base-consensus-upgrades`

<a href="https://crates.io/crates/base-consensus-upgrades"><img src="https://img.shields.io/crates/v/base-consensus-upgrades.svg" alt="base-consensus-upgrades crate"></a>
<a href="https://specs.base.org"><img src="https://img.shields.io/badge/Docs-854a15?style=flat&labelColor=1C2C2E&color=BEC5C9&logo=mdBook&logoColor=BEC5C9" alt="Docs" /></a>

Consensus layer upgrade types for Base including network upgrade transactions.

## Overview

Provides typed upgrade abstractions for the Base consensus layer. Defines the `Upgrade`
trait and an `Upgrades` registry, with concrete implementations for each upgrade (Ecotone, Fjord,
Isthmus, Jovian) including the network upgrade transactions that must be injected at upgrade
activation blocks.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-consensus-upgrades = { workspace = true }
```

```rust,ignore
use base_consensus_upgrades::{Upgrade, Upgrades};

let upgrades = Upgrades::default();
if upgrades.is_active::<Jovian>(timestamp) {
    // apply Jovian upgrade transactions
}
```

## Provenance

This code was ported from [op-alloy] as part of the `base` monorepo.

[op-alloy]: https://github.com/alloy-rs/op-alloy

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
