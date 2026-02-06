# `base-consensus-rpc`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

RPC types and client for Base. Provides jsonrpsee-based RPC trait definitions for Optimism rollup node APIs.

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
    async fn op_sync_status(&self) -> RpcResult<SyncStatus> {
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

let status = client.op_sync_status().await?;
```
