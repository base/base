# `base-infra-audit-service`

Audit library for tracking and archiving bundle events.

## Overview

Provides event publishing, storage, and retrieval for bundle lifecycle events. `AuditConnector`
wires an event receiver to a publisher, `RpcBundleEventPublisher` publishes events over RPC,
and `S3EventReaderWriter` archives events to S3 for long-term retention. Also exposes
`LoggingBundleEventPublisher` for local development.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-infra-audit-service = { workspace = true }
```

```rust,ignore
use base_infra_audit_service::{AuditConnector, RpcBundleEventPublisher};

let publisher = RpcBundleEventPublisher::new(rpc_url, timeout)?;
AuditConnector::connect_batched(event_rx, publisher, batch_size, batch_wait);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
