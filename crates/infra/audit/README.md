# `audit-archiver-lib`

Audit library for tracking and archiving bundle events.

## Overview

Provides event publishing, storage, and retrieval for bundle lifecycle events. `AuditConnector`
wires an event receiver to a publisher, `KafkaBundleEventPublisher` publishes events to Kafka,
and `S3EventReaderWriter` archives events to S3 for long-term retention. `KafkaAuditLogReader`
enables replaying the event history. Also exposes `LoggingBundleEventPublisher` for local
development.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
audit-archiver-lib = { workspace = true }
```

```rust,ignore
use audit_archiver_lib::{AuditConnector, KafkaBundleEventPublisher};

let publisher = KafkaBundleEventPublisher::new(kafka_config).await?;
let connector = AuditConnector::new(event_rx, publisher);
connector.run().await;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
