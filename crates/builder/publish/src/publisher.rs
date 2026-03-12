use core::fmt::{Debug, Formatter};
use std::{io, net::SocketAddr, sync::Arc};

use base_ring_buffer::RingBuffer;
use parking_lot::RwLock;
use serde::Serialize;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Utf8Bytes;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{Listener, PublisherMetrics, PublishingMetrics};

/// Position key for a flashblock: `(block_number, flashblock_index)`.
pub type FlashblockPosition = (u64, u64);

/// A broadcast payload paired with its position.
pub type PositionedPayload = (FlashblockPosition, Utf8Bytes);

/// Default broadcast channel capacity.
const DEFAULT_CHANNEL_CAPACITY: usize = 100;

/// Default ring buffer capacity (number of retained entries for replay).
const DEFAULT_RING_BUFFER_CAPACITY: usize = 16;

/// A WebSocket publisher that accepts connections from clients and broadcasts
/// serialized messages to all connected subscribers.
///
/// The publisher is generic over the payload type — any `Serialize` value can
/// be broadcast via [`WebSocketPublisher::publish`].
///
/// On publish, each message is stored in an internal ring buffer so that
/// reconnecting clients can request replay of recent entries.
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

    /// Creates a new `WebSocketPublisher` with custom broadcast channel and
    /// ring buffer capacities.
    ///
    /// The channel capacity determines how many messages can be buffered before
    /// slow subscribers begin lagging. The ring buffer capacity controls how
    /// many recent messages are retained for reconnection replay.
    pub fn with_capacity(
        addr: SocketAddr,
        channel_capacity: usize,
        ring_buffer_capacity: usize,
    ) -> io::Result<Self> {
        let (pipe, _) = broadcast::channel(channel_capacity);
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
                pipe.subscribe(),
                Arc::clone(&ring_buffer),
                cancel.child_token(),
            )
            .run(),
        );

        Ok(Self { cancel, pipe, metrics, ring_buffer })
    }

    /// Serializes the payload to JSON and broadcasts it to all connected
    /// subscribers.
    ///
    /// The entry is stored in the ring buffer first, keyed by
    /// `(block_number, flashblock_index)`, then broadcast to all receivers.
    ///
    /// Returns the byte size of the serialized payload on success.
    pub fn publish(
        &self,
        payload: &impl Serialize,
        block_number: u64,
        flashblock_index: u64,
    ) -> io::Result<usize> {
        let json = serde_json::to_string(payload)?;
        let size = json.len();
        let utf8_bytes = Utf8Bytes::from(json);
        let position = (block_number, flashblock_index);

        {
            let mut buf = self.ring_buffer.write();
            buf.push(Some(position), utf8_bytes.clone());
        }

        // Ignore SendError — there may be zero receivers when no clients
        // are connected. The entry is already stored in the ring buffer
        // for replay on future connections.
        let _ = self.pipe.send((position, utf8_bytes));
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

        // The listener task holds one receiver, so send should succeed.
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
    async fn ring_buffer_stores_entries() {
        let addr = ephemeral_addr();
        let publisher = WebSocketPublisher::with_capacity(addr, 8, 4).unwrap();

        publisher.publish(&serde_json::json!({"n": 1}), 100, 0).unwrap();
        publisher.publish(&serde_json::json!({"n": 2}), 100, 1).unwrap();
        publisher.publish(&serde_json::json!({"n": 3}), 101, 0).unwrap();

        let buf = publisher.ring_buffer.read();
        assert_eq!(buf.len(), 3);

        let entries: Vec<_> = buf.entries_after(&(100, 0)).collect();
        assert_eq!(entries.len(), 2);
    }
}
