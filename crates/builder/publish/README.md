# `base-builder-publish`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

WebSocket broadcast publisher for the OP Stack block builder. Accepts WebSocket connections from clients and broadcasts serialized payloads to all connected subscribers.

## Overview

- **`WebSocketPublisher`**: Top-level API that binds a TCP listener, accepts WebSocket connections, and broadcasts serialized messages to all subscribers. Metrics are registered automatically.
- **`Listener`**: Accepts incoming TCP connections, upgrades them to WebSocket, and spawns a `BroadcastLoop` for each client. Parses `?block_number=N&flashblock_index=M` from the HTTP upgrade request to support replay on reconnect.
- **`BroadcastLoop`**: Per-client send loop that replays ring-buffered entries after the client's resume position, then forwards live broadcast messages.
- **`RingBuffer`**: Fixed-capacity FIFO of recent flashblocks. Oldest entries are evicted when the buffer is full. Used to replay missed entries to reconnecting clients.
- **`PublishingMetrics`**: Concrete metrics implementation backed by the `metrics` crate, registered under the `base_builder` scope.
- **`PublisherMetrics`**: Trait abstracting metrics collection for message sends, connection lifecycle, and lag detection.
- **`NoopPublisherMetrics`**: No-op implementation of `PublisherMetrics` for testing.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-builder-publish = { git = "https://github.com/base/base" }
```

### Basic broadcast

```rust,ignore
use base_builder_publish::WebSocketPublisher;

let publisher = WebSocketPublisher::new("127.0.0.1:9999".parse().unwrap())?;

// Broadcast any Serialize value; returns the byte size of the payload.
publisher.publish(&serde_json::json!({"hello": "world"}))?;
```

### Flashblock publish with ring buffer

Use `publish_flashblock` to record a `(block_number, flashblock_index)` position alongside
each payload. This enables reconnecting clients to replay the messages they missed.

```rust,ignore
use base_builder_publish::WebSocketPublisher;

let publisher = WebSocketPublisher::new("127.0.0.1:9999".parse().unwrap())?;

// Publishes to the ring buffer keyed by position, then broadcasts live.
publisher.publish_flashblock(&payload, block_number, flashblock_index)?;
```

### Custom capacities

```rust,ignore
// channel_capacity: live broadcast buffer (messages in flight to slow clients)
// ring_buffer_capacity: how many recent flashblocks to keep for replay
let publisher = WebSocketPublisher::with_capacity(addr, 256, 55)?;
```

### Reconnect / resume

Clients can request replay of missed flashblocks by including `?block_number=N&flashblock_index=M`
as query parameters on their WebSocket upgrade request. The publisher will send all buffered entries
after that position before the client joins the live stream.

Example connect URL (client side):
```text
ws://127.0.0.1:9999/?block_number=42&flashblock_index=3
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
