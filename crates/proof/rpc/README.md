# `base-proof-rpc`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Shared RPC client abstractions for Base L1, L2, and rollup node interactions.

## Overview

Provides async client traits and concrete Alloy-backed implementations for:

- **`L1Client`**: Querying Ethereum L1 headers, receipts, balances, and contract state.
- **`L2Client`**: Querying Base L2 blocks, headers, proofs, and chain config.
- **`RollupClient`**: Querying rollup configuration and sync status from op-node.

Also provides a `MeteredCache` with hit/miss tracking, `RetryConfig` for exponential
backoff, shared RPC types (`BaseBlock`, `L1BlockRef`, `L2BlockRef`, `SyncStatus`), and
an `RpcError` type for error handling.

These abstractions are used by both [`base-proposer`](../proposer/) and the challenger.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-proof-rpc = { workspace = true }
```

Instantiate the Alloy-backed clients to query L1, L2, and rollup nodes:

```rust,ignore
use base_proof_rpc::{L1Client, L2Client, RollupClient};

let l1 = L1Client::new(l1_url)?;
let l2 = L2Client::new(l2_url)?;
let rollup = RollupClient::new(rollup_url)?;
```

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
