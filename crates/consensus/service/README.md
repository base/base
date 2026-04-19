# `base-consensus-node`

<a href="https://crates.io/crates/base-consensus-node"><img src="https://img.shields.io/crates/v/base-consensus-node.svg" alt="base-consensus-node crate"></a>
<a href="https://specs.base.org"><img src="https://img.shields.io/badge/Docs-854a15?style=flat&labelColor=1C2C2E&color=BEC5C9&logo=mdBook&logoColor=BEC5C9" alt="Docs" /></a>

An implementation of the Base [RollupNode][rn-spec] service.

[rn-spec]: https://specs.base.org/protocol/consensus

## Overview

Orchestrates all Base rollup node subsystems — engine, gossip, discovery, derivation, and RPC
— into a single `RollupNode` service. Wires together the consensus engine client, P2P
networking stack, L1/L2 derivation pipeline, and admin RPC server, providing a unified
start/stop lifecycle.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-consensus-node = { workspace = true }
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
