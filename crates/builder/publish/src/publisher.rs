use core::fmt::{Debug, Formatter};
use std::{
    io,
    net::SocketAddr,
    sync::{Arc, RwLock},
};

use base_ring_buffer::RingBuffer;
use serde::Serialize;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Utf8Bytes;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Position of a flashblock entry in the stream.
pub type FlashblockPosition = (u64, u64);

/// A broadcast entry carrying an optional position alongside its payload.
pub type PositionedPayload = (Option<FlashblockPosition>, Utf8Bytes);

use crate::{Listener, PublisherMetrics, PublishingMetrics};

/// Default broadcast channel capacity.
const DEFAULT_CHANNEL_CAPACITY: usize = 100;

/// Default ring buffer capacity (~1.5 blocks at ~5 flashblocks/s per block).
const DEFAULT_RING_BUFFER_CAPACITY: usize = 16;

/// A WebSocket publisher that accepts connections from clients and broadcasts
/// serialized flashblock messages to all connected subscribers.
///
/// Dropping the publisher cancels the listener, gracefully closing all
/// connections.
pub struct WebSocketPublisher {
    cancel: CancellationToken,
    pipe: broadcast::Sender<PositionedPayload>,
    metrics: Arc<dyn PublisherMetrics>,
    ring_buffer: Arc<RwLock<RingBuffer<FlashblockPosition, Utf8Bytes>>>,
}

impl WebSocketPublisher {
    /// Creates a new `WebSocketPublisher` bound to the given address with the
    /// default broadcast channel capacity (100) and ring buffer capacity (16).
    ///
    /// Spawns a background listener task that accepts WebSocket connections
    /// and broadcasts messages published via [`Self::publish`]. Metrics are
    /// registered automatically under the `base_builder` scope.
    pub fn new(addr: SocketAddr) -> io::Result<Self> {
        Self::with_capacity(addr, DEFAULT_CHANNEL_CAPACITY, DEFAULT_RING_BUFFER_CAPACITY)
    }

    /// Creates a new `WebSocketPublisher` with custom broadcast channel and ring buffer capacities.
    ///
    /// The broadcast `capacity` determines how many messages can be buffered before slow
    /// subscribers begin lagging. The `ring_buffer_capacity` controls how many flashblocks
    /// are kept for reconnecting subscribers to replay.
    pub fn with_capacity(
        addr: SocketAddr,
        capacity: usize,
        ring_buffer_capacity: usize,
    ) -> io::Result<Self> {
        let (pipe, _) = broadcast::channel(capacity);
        let cancel = CancellationToken::new();
        let metrics: Arc<dyn PublisherMetrics> = Arc::new(PublishingMetrics::default());
        let ring_buffer = Arc::new(RwLock::new(RingBuffer::new(ring_buffer_capacity)));

        let std_listener = std::net::TcpListener::bind(addr)?;
        std_listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(std_listener)?;

        tokio::spawn(
            Listener::new(
                listener,
                Arc::clone(&metrics),
                pipe.clone(),
                cancel.child_token(),
                Arc::clone(&ring_buffer),
            )
            .run(),
        );

        Ok(Self { cancel, pipe, metrics, ring_buffer })
    }

    /// Serializes the payload to JSON, broadcasts to all connected subscribers,
    /// then stores it in the ring buffer keyed by `(block_number, flashblock_index)`.
    ///
    /// Reconnecting subscribers that supply `?block_number=N&flashblock_index=M` on
    /// their upgrade request will receive all buffered entries after that position
    /// before joining the live stream.
    ///
    /// Returns the byte size of the serialized payload on success.
    pub fn publish(
        &self,
        payload: &impl Serialize,
        block_number: u64,
        flashblock_index: u64,
    ) -> io::Result<usize> {
        let serialized = serde_json::to_string(payload)?;
        let utf8_bytes = Utf8Bytes::from(serialized);
        let size = utf8_bytes.len();
        let position = Some((block_number, flashblock_index));

        // Broadcast first so that live subscribers never miss a message that
        // exists in the ring buffer. `broadcast::Sender::send` only fails
        // with `SendError` when there are zero receivers, which is expected
        // when no clients are connected. There is no other failure mode.
        let _ = self.pipe.send((position, utf8_bytes.clone()));

        self.ring_buffer.write().unwrap_or_else(|e| e.into_inner()).push(position, utf8_bytes);

        self.metrics.on_payload_size(size);
        Ok(size)
    }
}

impl Drop for WebSocketPublisher {
    fn drop(&mut self) {
        self.cancel.cancel();
        info!("WebSocketPublisher dropped, terminating listener loop");
    }
}

impl Debug for WebSocketPublisher {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WebSocketPublisher")
            .field("subscribers", &self.pipe.receiver_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    use super::*;

    /// Binds an OS-assigned port, returns the address, then drops the listener
    /// so the publisher can rebind.
    fn ephemeral_addr() -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        addr
    }

    #[tokio::test]
    async fn publish_end_to_end() {
        let addr = ephemeral_addr();
        let publisher = WebSocketPublisher::new(addr).unwrap();

        let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let payload = serde_json::json!({"block": 42});
        let size = publisher.publish(&payload, 1, 0).unwrap();
        assert!(size > 0);

        let msg = client.next().await.unwrap().unwrap();
        assert_eq!(msg, Message::Text(Utf8Bytes::from(r#"{"block":42}"#)));
    }

    #[tokio::test]
    async fn publish_with_custom_capacity() {
        let addr = ephemeral_addr();
        let publisher = WebSocketPublisher::with_capacity(addr, 8, 4).unwrap();

        let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        publisher.publish(&serde_json::json!({"test": true}), 1, 0).unwrap();

        let msg = client.next().await.unwrap().unwrap();
        assert_eq!(msg, Message::Text(Utf8Bytes::from(r#"{"test":true}"#)));
    }

    #[tokio::test]
    async fn drop_shuts_down_listener() {
        let addr = ephemeral_addr();
        let publisher = WebSocketPublisher::new(addr).unwrap();

        let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        drop(publisher);

        // Client should receive a close or error once the publisher shuts down
        let result = tokio::time::timeout(Duration::from_secs(2), client.next()).await;
        assert!(result.is_ok(), "client should receive close notification");
    }

    #[tokio::test]
    async fn publish_with_zero_subscribers_succeeds() {
        let publisher = WebSocketPublisher::new("127.0.0.1:0".parse().unwrap()).unwrap();

        // No active receivers; broadcast is silently ignored, ring buffer is populated.
        let result = publisher.publish(&serde_json::json!({"test": true}), 1, 0);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn debug_impl_works() {
        let publisher = WebSocketPublisher::new("127.0.0.1:0".parse().unwrap()).unwrap();
        let debug_str = format!("{publisher:?}");
        assert!(debug_str.contains("WebSocketPublisher"));
    }

    #[tokio::test]
    async fn publish_stores_in_ring_buffer() {
        let publisher = WebSocketPublisher::new("127.0.0.1:0".parse().unwrap()).unwrap();
        let payload = serde_json::json!({"index": 1});

        publisher.publish(&payload, 42, 1).unwrap();

        let buf = publisher.ring_buffer.read().unwrap();
        assert_eq!(buf.entries_after(&(42, 0)).count(), 1);
    }

    #[tokio::test]
    async fn publish_broadcasts_live() {
        let addr = ephemeral_addr();
        let publisher = WebSocketPublisher::new(addr).unwrap();

        let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let payload = serde_json::json!({"index": 0, "metadata": {"block_number": 1}});
        publisher.publish(&payload, 1, 0).unwrap();

        let msg = client.next().await.unwrap().unwrap();
        if let Message::Text(text) = msg {
            assert!(text.contains("block_number"));
        } else {
            panic!("expected text message");
        }
    }
}
