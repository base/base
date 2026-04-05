# `base-alloy-rpc-jsonrpsee`

Base chain JSON-RPC server and client implementations.

## Overview

Provides jsonrpsee-based JSON-RPC trait definitions for Base chain admin and miner APIs.
`MinerApiExtServer` and `MinerApiExtClient` expose miner-side endpoints, while `BaseAdminApiServer`
exposes admin-side RPC methods. These traits are implemented by node components to expose Base-specific
JSON-RPC functionality over HTTP or WebSocket transports.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-alloy-rpc-jsonrpsee = { workspace = true }
```

```rust,ignore
use base_alloy_rpc_jsonrpsee::{BaseAdminApiServer, MinerApiExtServer};

#[async_trait]
impl MinerApiExtServer for MyHandler {
    async fn set_max_da_size(&self, size: u64) -> RpcResult<()> { .. }
}
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
