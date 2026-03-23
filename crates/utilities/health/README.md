# `base-health`

Shared health check utilities for Base services.

## JSON-RPC health endpoint

Provides a `HealthzApi` trait and `HealthzRpc` implementation that returns
the crate version via a `healthz` JSON-RPC method. Designed to work with
`jsonrpsee`'s `ProxyGetRequestLayer` to expose `GET /healthz` on the same
port as the RPC server.

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

## Standalone health server (`axum-server` feature)

A lightweight axum-based HTTP server with Kubernetes-style health probes:

- `GET /healthz` — liveness probe (always returns 200)
- `GET /readyz`  — readiness probe (returns 200 when ready, 503 otherwise)

Enable the feature:

```toml
[dependencies]
base-health = { git = "https://github.com/base/base", features = ["axum-server"] }
```

### Standalone server

```rust,ignore
use std::sync::{Arc, atomic::AtomicBool};
use base_health::HealthServer;
use tokio_util::sync::CancellationToken;

let ready = Arc::new(AtomicBool::new(false));
let cancel = CancellationToken::new();

// Spawns an HTTP server on 0.0.0.0:8080
HealthServer::serve("0.0.0.0:8080".parse().unwrap(), ready, cancel).await?;
```

### Custom listener setup

Use `HealthServer::router()` when you need a custom listener or want to add
middleware. The returned `Router` has its state already applied (`Router<()>`),
so it can be composed with other routers or middleware as needed:

```rust,ignore
use base_health::HealthServer;

let app = HealthServer::router(ready);
let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
axum::serve(listener, app).await?;
```
