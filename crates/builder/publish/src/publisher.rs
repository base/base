use core::fmt::{Debug, Formatter};
use std::{net::SocketAddr, sync::Arc};

use serde::Serialize;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{Listener, PublisherMetrics, PublishingMetrics};

/// Default broadcast channel capacity.
const DEFAULT_CHANNEL_CAPACITY: usize = 100;

/// A WebSocket publisher that accepts connections from clients and broadcasts
/// serialized messages to all connected subscribers.
///
/// The publisher is generic over the payload type — any `Serialize` value can
/// be broadcast via [`WebSocketPublisher::publish`]. Delivery is live-only:
/// clients receive messages published while they are connected.
///
/// Dropping the publisher cancels the listener, gracefully closing all
/// connections.
pub struct WebSocketPublisher {
    cancel: CancellationToken,
    pipe: broadcast::Sender<Message>,
    metrics: Arc<dyn PublisherMetrics>,
}

impl WebSocketPublisher {
    /// Creates a new `WebSocketPublisher` bound to the given address with the
    /// default broadcast channel capacity (100).
    ///
    /// Spawns a background listener task that accepts WebSocket connections
    /// and broadcasts messages published via [`Self::publish`]. Metrics are
    /// registered automatically under the `base_builder` scope.
    pub fn new(addr: SocketAddr) -> std::io::Result<Self> {
        Self::with_capacity(addr, DEFAULT_CHANNEL_CAPACITY)
    }

    /// Creates a new `WebSocketPublisher` with a custom broadcast channel
    /// capacity.
    ///
    /// The channel capacity determines how many messages can be buffered before
    /// slow subscribers begin lagging.
    pub fn with_capacity(addr: SocketAddr, channel_capacity: usize) -> std::io::Result<Self> {
        let (pipe, _) = broadcast::channel(channel_capacity);
        let cancel = CancellationToken::new();
        let metrics: Arc<dyn PublisherMetrics> = Arc::new(PublishingMetrics);

        let std_listener = std::net::TcpListener::bind(addr)?;
        std_listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(std_listener)?;

        tokio::spawn(
            Listener::new(listener, Arc::clone(&metrics), pipe.subscribe(), cancel.child_token())
                .run(),
        );

        Ok(Self { cancel, pipe, metrics })
    }

    /// Serializes the payload to JSON and broadcasts it to all connected
    /// subscribers.
    ///
    /// Returns the byte size of the serialized payload on success.
    pub fn publish(&self, payload: &impl Serialize) -> Result<usize, serde_json::Error> {
        Ok(self.publish_message(Message::text(serde_json::to_string(payload)?)))
    }

    /// Broadcasts an already-encoded text or binary message, like [`Self::publish`].
    ///
    /// Returns the byte size of the message.
    pub fn publish_message(&self, message: Message) -> usize {
        let size = message.len();
        // Ignore SendError — there may be zero receivers when no clients
        // are connected.
        let _ = self.pipe.send(message);
        self.metrics.on_payload_size(size);
        size
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
    use tokio_tungstenite::connect_async;

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
        let size = publisher.publish(&payload).unwrap();
        assert!(size > 0);

        let msg = client.next().await.unwrap().unwrap();
        assert_eq!(msg, Message::text(r#"{"block":42}"#));
    }

    #[tokio::test]
    async fn publish_binary_message_end_to_end() {
        let addr = ephemeral_addr();
        let publisher = WebSocketPublisher::new(addr).unwrap();

        let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let size = publisher.publish_message(Message::binary(vec![1u8, 2, 3]));
        assert_eq!(size, 3);

        let msg = client.next().await.unwrap().unwrap();
        assert_eq!(msg, Message::binary(vec![1u8, 2, 3]));
    }

    #[tokio::test]
    async fn publish_with_custom_capacity() {
        let addr = ephemeral_addr();
        let publisher = WebSocketPublisher::with_capacity(addr, 8).unwrap();

        let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        publisher.publish(&serde_json::json!({"test": true})).unwrap();

        let msg = client.next().await.unwrap().unwrap();
        assert_eq!(msg, Message::text(r#"{"test":true}"#));
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
        let result = publisher.publish(&serde_json::json!({"test": true}));
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn debug_impl_works() {
        let publisher = WebSocketPublisher::new("127.0.0.1:0".parse().unwrap()).unwrap();
        let debug_str = format!("{publisher:?}");
        assert!(debug_str.contains("WebSocketPublisher"));
    }
}
