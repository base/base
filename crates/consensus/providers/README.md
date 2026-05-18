# `base-consensus-providers`

<a href="https://crates.io/crates/base-consensus-providers"><img src="https://img.shields.io/crates/v/base-consensus-providers.svg?label=base-consensus-providers&labelColor=2a2f35" alt="base-consensus-providers"></a>

Alloy-backed providers for the Base consensus node.

## Overview

Implements data-fetching providers backed by alloy RPC clients for use during L2 derivation.
Includes `AlloyChainProvider` for L1 block and receipt fetching, `AlloyL2ChainProvider` for L2
block and system config access, `OnlineBeaconClient` for Ethereum beacon API queries, and
`OnlineBlobProvider` for fetching EIP-4844 blob sidecars. Also exports `OnlinePipeline` for
constructing a fully wired derivation pipeline against live nodes.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-consensus-providers = { workspace = true }
```

```rust,ignore
use base_consensus_providers::{AlloyChainProvider, AlloyL2ChainProvider};

let l1_provider = AlloyChainProvider::new(l1_rpc_url);
let l2_provider = AlloyL2ChainProvider::new(l2_rpc_url, rollup_config);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
