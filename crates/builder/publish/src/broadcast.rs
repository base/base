use core::fmt::{Debug, Formatter};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use tokio::{
    net::TcpStream,
    sync::broadcast::{Receiver, error::RecvError},
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::PublisherMetrics;

/// Per-client broadcast loop.
///
/// Created for each connected WebSocket client. Forwards live broadcast
/// messages to the client until cancellation, disconnect, or lag.
pub struct BroadcastLoop {
    stream: WebSocketStream<TcpStream>,
    cancel: CancellationToken,
    metrics: Arc<dyn PublisherMetrics>,
    receiver: Receiver<Message>,
}

impl Debug for BroadcastLoop {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BroadcastLoop").finish_non_exhaustive()
    }
}

impl BroadcastLoop {
    /// Creates a new [`BroadcastLoop`].
    pub const fn new(
        stream: WebSocketStream<TcpStream>,
        metrics: Arc<dyn PublisherMetrics>,
        cancel: CancellationToken,
        receiver: Receiver<Message>,
    ) -> Self {
        Self { stream, metrics, cancel, receiver }
    }

    /// Runs the broadcast loop until cancellation or error.
    pub async fn run(mut self) {
        let Ok(peer_addr) = self.stream.get_ref().peer_addr() else {
            return;
        };

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("WebSocketPublisher is terminating, closing broadcast loop");
                    return;
                }

                payload = self.receiver.recv() => match payload {
                    Ok(data) => {
                        debug!(payload = ?data, "Broadcasted payload");
                        if let Err(e) = self.stream.send(data).await {
                            self.metrics.on_send_error();
                            debug!(peer_addr = %peer_addr, error = %e, "Closing subscription");
                            break;
                        }
                        self.metrics.on_message_sent();
                    }
                    Err(RecvError::Closed) => {
                        debug!("Broadcast channel closed, exiting broadcast loop");
                        return;
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        self.metrics.on_lagged(skipped);
                        warn!(skipped = skipped, "Broadcast channel lagged, disconnecting client");
                        break;
                    }
                },

                message = self.stream.next() => if let Some(message) = message { match message {
                    Ok(Message::Close(_)) => {
                        info!(peer_addr = %peer_addr, "Closing frame received, stopping connection");
                        break;
                    }
                    Err(e) => {
                        warn!(peer_addr = %peer_addr, error = %e, "Received error. Closing subscription");
                        break;
                    }
                    _ => (),
                } }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::{net::TcpListener, sync::broadcast};
    use tokio_tungstenite::{accept_async, connect_async};

    use super::*;

    struct MockMetrics {
        sent: AtomicU64,
        lagged: AtomicU64,
    }
    impl MockMetrics {
        fn new() -> Self {
            Self { sent: AtomicU64::new(0), lagged: AtomicU64::new(0) }
        }
    }

    impl crate::PublisherMetrics for MockMetrics {
        fn on_message_sent(&self) {
            self.sent.fetch_add(1, Ordering::Relaxed);
        }
        fn on_connection_opened(&self) {}
        fn on_connection_closed(&self, _duration: std::time::Duration) {}
        fn on_lagged(&self, skipped: u64) {
            self.lagged.fetch_add(skipped, Ordering::Relaxed);
        }
        fn on_payload_size(&self, _size: usize) {}
        fn on_send_error(&self) {}
        fn on_handshake_error(&self) {}
    }

    #[tokio::test]
    async fn broadcast_loop_forwards_messages() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = broadcast::channel::<Message>(16);
        let cancel = CancellationToken::new();
        let metrics: Arc<dyn crate::PublisherMetrics> = Arc::new(MockMetrics::new());

        let server_handle = tokio::spawn({
            let metrics = Arc::clone(&metrics);
            let cancel = cancel.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let ws = accept_async(stream).await.unwrap();
                BroadcastLoop::new(ws, metrics, cancel, rx).run().await;
            }
        });

        let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        tx.send(Message::text("hello")).unwrap();

        let msg = client.next().await.unwrap().unwrap();
        assert_eq!(msg, Message::text("hello"));

        cancel.cancel();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn broadcast_loop_exits_on_cancellation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_tx, rx) = broadcast::channel::<Message>(16);
        let cancel = CancellationToken::new();
        let metrics: Arc<dyn crate::PublisherMetrics> = Arc::new(MockMetrics::new());

        let server_handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let ws = accept_async(stream).await.unwrap();
                BroadcastLoop::new(ws, metrics, cancel, rx).run().await;
            }
        });

        let (_client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        cancel.cancel();
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn broadcast_loop_exits_on_close_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_tx, rx) = broadcast::channel::<Message>(16);
        let cancel = CancellationToken::new();
        let metrics: Arc<dyn crate::PublisherMetrics> = Arc::new(MockMetrics::new());

        let server_handle = tokio::spawn({
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let ws = accept_async(stream).await.unwrap();
                BroadcastLoop::new(ws, metrics, cancel, rx).run().await;
            }
        });

        let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        client.close(None).await.unwrap();

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn broadcast_loop_handles_lagged_receiver() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (tx, rx) = broadcast::channel::<Message>(1);
        let cancel = CancellationToken::new();
        let metrics = Arc::new(MockMetrics::new());
        let metrics_clone: Arc<dyn crate::PublisherMetrics> =
            Arc::clone(&metrics) as Arc<dyn crate::PublisherMetrics>;

        let server_handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let ws = accept_async(stream).await.unwrap();
                BroadcastLoop::new(ws, metrics_clone, cancel, rx).run().await;
            }
        });

        let (_client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        for i in 0..5 {
            let _ = tx.send(Message::text(format!("msg{i}")));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(metrics.lagged.load(Ordering::Relaxed) > 0);
        cancel.cancel();

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn broadcast_loop_exits_on_channel_closed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = broadcast::channel::<Message>(16);
        let cancel = CancellationToken::new();
        let metrics: Arc<dyn crate::PublisherMetrics> = Arc::new(MockMetrics::new());

        let server_handle = tokio::spawn({
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let ws = accept_async(stream).await.unwrap();
                BroadcastLoop::new(ws, metrics, cancel, rx).run().await;
            }
        });

        let (_client, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Drop the sender to close the broadcast channel.
        drop(tx);

        server_handle.await.unwrap();
    }
}
