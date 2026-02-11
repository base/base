# `base-builder-publish`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

WebSocket broadcast publisher for the OP Stack block builder. Accepts WebSocket connections from clients and broadcasts serialized payloads to all connected subscribers.

## Overview

- **`WebSocketPublisher`**: Top-level API that binds a TCP listener, accepts WebSocket connections, and broadcasts serialized messages to all subscribers. Metrics are registered automatically.
- **`Listener`**: Accepts incoming TCP connections, upgrades them to WebSocket, and spawns a `BroadcastLoop` for each client.
- **`BroadcastLoop`**: Per-client send loop that forwards broadcast messages to a single WebSocket stream.
- **`PublishingMetrics`**: Concrete metrics implementation backed by the `metrics` crate, registered under the `base_builder` scope.
- **`PublisherMetrics`**: Trait abstracting metrics collection for message sends, connection lifecycle, and lag detection.
- **`NoopPublisherMetrics`**: No-op implementation of `PublisherMetrics` for testing.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-builder-publish = { git = "https://github.com/base/base" }
```

Create a publisher and broadcast messages:

```rust,ignore
use base_builder_publish::WebSocketPublisher;

// Default channel capacity (100)
let publisher = WebSocketPublisher::new("127.0.0.1:9999".parse().unwrap())?;
publisher.publish(&serde_json::json!({"hello": "world"}))?;

// Or with a custom channel capacity
let publisher = WebSocketPublisher::with_capacity("127.0.0.1:9999".parse().unwrap(), 256)?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
