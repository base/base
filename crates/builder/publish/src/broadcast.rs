use core::fmt::{Debug, Formatter};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use tokio::{
    net::TcpStream,
    sync::broadcast::{self, error::RecvError},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, Utf8Bytes},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::PublisherMetrics;

/// Per-client broadcast sender.
///
/// Created for each connected WebSocket client. Forwards messages from the
/// broadcast channel to the client's WebSocket stream. Exits on cancellation,
/// channel close, or WebSocket error.
pub struct BroadcastLoop {
    stream: WebSocketStream<TcpStream>,
    metrics: Arc<dyn PublisherMetrics>,
    cancel: CancellationToken,
    blocks: broadcast::Receiver<Utf8Bytes>,
}

impl Debug for BroadcastLoop {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BroadcastLoop").finish_non_exhaustive()
    }
}

impl BroadcastLoop {
    /// Creates a new [`BroadcastLoop`].
    pub fn new(
        stream: WebSocketStream<TcpStream>,
        metrics: Arc<dyn PublisherMetrics>,
        cancel: CancellationToken,
        blocks: broadcast::Receiver<Utf8Bytes>,
    ) -> Self {
        Self { stream, metrics, cancel, blocks }
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

                payload = self.blocks.recv() => match payload {
                    Ok(payload) => {
                        self.metrics.on_message_sent();

                        debug!(payload = ?payload, "Broadcasted payload");
                        if let Err(e) = self.stream.send(Message::Text(payload)).await {
                            self.metrics.on_send_error();
                            debug!(peer_addr = %peer_addr, error = %e, "Closing subscription");
                            break;
                        }
                    }
                    Err(RecvError::Closed) => {
                        debug!("Broadcast channel closed, exiting broadcast loop");
                        return;
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        self.metrics.on_lagged(skipped);
                        warn!(
                            skipped = skipped,
                            "Broadcast channel lagged, some messages were dropped"
                        );
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use futures::StreamExt;
    use rstest::rstest;
    use tokio::net::TcpListener;
    use tokio::sync::broadcast;
    use tokio_tungstenite::{
        accept_async, connect_async,
        tungstenite::{Message, Utf8Bytes},
    };
    use tokio_util::sync::CancellationToken;

    use super::BroadcastLoop;
    use crate::PublisherMetrics;

    struct MockMetrics {
        sent: AtomicU64,
        lagged: AtomicU64,
    }

    impl MockMetrics {
        fn new() -> Self {
            Self { sent: AtomicU64::new(0), lagged: AtomicU64::new(0) }
        }
    }

    impl PublisherMetrics for MockMetrics {
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
        let (tx, rx) = broadcast::channel::<Utf8Bytes>(16);
        let cancel = CancellationToken::new();
        let metrics: Arc<dyn PublisherMetrics> = Arc::new(MockMetrics::new());

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

        tx.send(Utf8Bytes::from("hello")).unwrap();

        let msg = client.next().await.unwrap().unwrap();
        assert_eq!(msg, Message::Text(Utf8Bytes::from("hello")));

        cancel.cancel();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn broadcast_loop_exits_on_cancellation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_, rx) = broadcast::channel::<Utf8Bytes>(16);
        let cancel = CancellationToken::new();
        let metrics: Arc<dyn PublisherMetrics> = Arc::new(MockMetrics::new());

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
        let (_tx, rx) = broadcast::channel::<Utf8Bytes>(16);
        let cancel = CancellationToken::new();
        let metrics: Arc<dyn PublisherMetrics> = Arc::new(MockMetrics::new());

        let server_handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = accept_async(stream).await.unwrap();
            BroadcastLoop::new(ws, metrics, cancel, rx).run().await;
        });

        let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        client.close(None).await.unwrap();

        server_handle.await.unwrap();
    }

    #[rstest]
    #[case::lagged(true)]
    #[case::closed(false)]
    #[tokio::test]
    async fn broadcast_loop_handles_recv_errors(#[case] test_lagged: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (tx, rx) = broadcast::channel::<Utf8Bytes>(1);
        let cancel = CancellationToken::new();
        let metrics = Arc::new(MockMetrics::new());
        let metrics_clone: Arc<dyn PublisherMetrics> =
            Arc::clone(&metrics) as Arc<dyn PublisherMetrics>;

        let server_handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let ws = accept_async(stream).await.unwrap();
                BroadcastLoop::new(ws, metrics_clone, cancel, rx).run().await;
            }
        });

        let (_client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        if test_lagged {
            for i in 0..5 {
                let _ = tx.send(Utf8Bytes::from(format!("msg{i}")));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert!(metrics.lagged.load(Ordering::Relaxed) > 0);
            cancel.cancel();
        } else {
            drop(tx);
        }

        server_handle.await.unwrap();
    }
}
