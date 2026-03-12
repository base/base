# `base-health`

Shared health check RPC endpoint for JSON-RPC servers.

Provides a `HealthzApi` trait and `HealthzRpc` implementation that returns
the crate version via a `healthz` JSON-RPC method. Designed to work with
`jsonrpsee`'s `ProxyGetRequestLayer` to expose `GET /healthz` on the same
port as the RPC server.

## Usage

```toml
[dependencies]
base-health = { git = "https://github.com/base/base" }
```

```rust,ignore
use base_health::{HealthzApiServer, HealthzRpc};
use jsonrpsee::RpcModule;

let mut module = RpcModule::new(());
module.merge(HealthzRpc::new(env!("CARGO_PKG_VERSION")).into_rpc())?;
```
