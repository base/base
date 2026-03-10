# `ingress-rpc-lib`

Ingress RPC library.

## Overview

Handles incoming transaction and bundle submission for the Base block builder pipeline.
`IngressService` exposes a JSON-RPC endpoint that validates bundles (`validate_bundle`),
meters them via `BuilderConnector`, and routes accepted transactions to Kafka
(`KafkaMessageQueue`) or the mempool. Also provides `HealthServer` for liveness checks and
`Metrics` for request tracking.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
ingress-rpc-lib = { workspace = true }
```

```rust,ignore
use ingress_rpc_lib::{IngressService, Config};

let service = IngressService::new(Config::from_env()?);
service.run().await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
