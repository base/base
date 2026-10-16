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
use std::sync::Arc;

use alloy_provider::RootProvider;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_consensus_providers::{AlloyChainProvider, AlloyL2ChainProvider};

let cache_size = 1024;
let l1_rpc = RootProvider::new_http(l1_rpc_url);
let l2_rpc = RootProvider::<Base>::new_http(l2_rpc_url);
let rollup_config: Arc<RollupConfig> = Arc::new(rollup_config);

let l1_provider = AlloyChainProvider::new(l1_rpc, cache_size);
let l2_provider = AlloyL2ChainProvider::new(l2_rpc, rollup_config, cache_size);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
