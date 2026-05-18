# `base-consensus-rpc`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

## Overview

jsonrpsee trait definitions for the Base rollup node RPC API. Provides `SyncStatusApiServer`
and `SyncStatusApiClient` for the `optimism_syncStatus` method, which returns current L1 and L2
block references (unsafe, safe, and finalized heads). Enable the `client` feature for the
generated HTTP client.

## RPC Methods

### `optimism_syncStatus`

Returns the current sync status of the node.

**Parameters:** None

**Returns:**
- `SyncStatus`: Current L1/L2 block references including unsafe, safe, and finalized heads.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-consensus-rpc = { git = "https://github.com/base/base" }
```

### Server Implementation

Implement the `SyncStatusApiServer` trait for your RPC handler:

```rust,ignore
use base_consensus_rpc::SyncStatusApiServer;
use base_protocol::SyncStatus;
use jsonrpsee::core::RpcResult;

struct MyRpcHandler;

#[async_trait::async_trait]
impl SyncStatusApiServer for MyRpcHandler {
    async fn sync_status(&self) -> RpcResult<SyncStatus> {
        // Return current sync status
        Ok(SyncStatus::default())
    }
}
```

### Client Usage

Enable the `client` feature to use the generated RPC client:

```toml
[dependencies]
base-consensus-rpc = { git = "https://github.com/base/base", features = ["client"] }
```

```rust,ignore
use base_consensus_rpc::SyncStatusApiClient;
use jsonrpsee::http_client::HttpClientBuilder;

let client = HttpClientBuilder::default()
    .build("http://localhost:8545")?;

let status = client.sync_status().await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
