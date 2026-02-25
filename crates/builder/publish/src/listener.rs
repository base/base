use core::fmt::{Debug, Formatter};
use std::sync::Arc;

use tokio::{net::TcpListener, sync::broadcast::Receiver};
use tokio_tungstenite::{accept_async, tungstenite::Utf8Bytes};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{BroadcastLoop, PublisherMetrics};

/// WebSocket connection listener.
///
/// Accepts incoming TCP connections, upgrades them to WebSocket, and spawns
/// a [`BroadcastLoop`] for each connected client.
pub struct Listener {
    listener: TcpListener,
    metrics: Arc<dyn PublisherMetrics>,
    receiver: Receiver<Utf8Bytes>,
    cancel: CancellationToken,
}

impl Debug for Listener {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Listener")
            .field("addr", &self.listener.local_addr())
            .finish_non_exhaustive()
    }
}

impl Listener {
    /// Creates a new [`Listener`] from an already-bound [`TcpListener`].
    pub fn new(
        listener: TcpListener,
        metrics: Arc<dyn PublisherMetrics>,
        receiver: Receiver<Utf8Bytes>,
        cancel: CancellationToken,
    ) -> Self {
        Self { listener, metrics, receiver, cancel }
    }

    /// Runs the listener loop, accepting connections until cancelled.
    pub async fn run(self) {
        let Self { listener, metrics, receiver, cancel } = self;

        let listen_addr =
            listener.local_addr().map(|a| a.to_string()).unwrap_or_else(|_| "unknown".into());
        info!(addr = %listen_addr, "WebSocketPublisher listening");

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    return;
                }

                result = listener.accept() => {
                    let Ok((connection, peer_addr)) = result else {
                        continue;
                    };

                    let cancel = cancel.clone();
                    let receiver_clone = receiver.resubscribe();
                    let metrics = Arc::clone(&metrics);

                    match accept_async(connection).await {
                        Ok(stream) => {
                            tokio::spawn({
                                let metrics = Arc::clone(&metrics);
                                async move {
                                    metrics.on_connection_opened();
                                    let connected_at = std::time::Instant::now();
                                    debug!(peer_addr = %peer_addr, "WebSocket connection established");

                                    BroadcastLoop::new(
                                        stream,
                                        Arc::clone(&metrics),
                                        cancel,
                                        receiver_clone,
                                    )
                                    .run()
                                    .await;

                                    metrics.on_connection_closed(connected_at.elapsed());
                                    debug!(peer_addr = %peer_addr, "WebSocket connection closed");
                                }
                            });
                        }
                        Err(e) => {
                            metrics.on_handshake_error();
                            warn!(peer_addr = %peer_addr, error = %e, "Failed to accept WebSocket connection");
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::Arc,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    use futures::StreamExt;
    use tokio::net::TcpListener;
    use tokio::sync::broadcast;
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{Message, Utf8Bytes},
    };
    use tokio_util::sync::CancellationToken;

    use super::Listener;
    use crate::PublisherMetrics;

    struct MockMetrics {
        opened: AtomicU64,
        closed: AtomicU64,
        sent: AtomicU64,
    }

    impl MockMetrics {
        fn new() -> Self {
            Self { opened: AtomicU64::new(0), closed: AtomicU64::new(0), sent: AtomicU64::new(0) }
        }
    }

    impl PublisherMetrics for MockMetrics {
        fn on_message_sent(&self) {
            self.sent.fetch_add(1, Ordering::Relaxed);
        }
        fn on_connection_opened(&self) {
            self.opened.fetch_add(1, Ordering::Relaxed);
        }
        fn on_connection_closed(&self, _duration: Duration) {
            self.closed.fetch_add(1, Ordering::Relaxed);
        }
        fn on_lagged(&self, _skipped: u64) {}
        fn on_payload_size(&self, _size: usize) {}
        fn on_send_error(&self) {}
        fn on_handshake_error(&self) {}
    }

    async fn bind_listener() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    #[tokio::test]
    async fn listener_accepts_and_receives_message() {
        let (listener, addr) = bind_listener().await;
        let (tx, rx) = broadcast::channel::<Utf8Bytes>(16);
        let cancel = CancellationToken::new();
        let metrics = Arc::new(MockMetrics::new());

        let handle = tokio::spawn({
            let metrics = Arc::clone(&metrics) as Arc<dyn PublisherMetrics>;
            let cancel = cancel.clone();
            async move {
                Listener::new(listener, metrics, rx, cancel).run().await;
            }
        });

        let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.send(Utf8Bytes::from("test-payload")).unwrap();

        let msg = client.next().await.unwrap().unwrap();
        assert_eq!(msg, Message::Text(Utf8Bytes::from("test-payload")));

        assert_eq!(metrics.opened.load(Ordering::Relaxed), 1);

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn listener_graceful_shutdown() {
        let (listener, _addr) = bind_listener().await;
        let (_, rx) = broadcast::channel::<Utf8Bytes>(16);
        let cancel = CancellationToken::new();
        let metrics: Arc<dyn PublisherMetrics> = Arc::new(MockMetrics::new());

        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                Listener::new(listener, metrics, rx, cancel).run().await;
            }
        });

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn listener_multiple_clients_receive_same_message() {
        let (listener, addr) = bind_listener().await;
        let (tx, rx) = broadcast::channel::<Utf8Bytes>(16);
        let cancel = CancellationToken::new();
        let metrics = Arc::new(MockMetrics::new());

        let handle = tokio::spawn({
            let metrics = Arc::clone(&metrics) as Arc<dyn PublisherMetrics>;
            let cancel = cancel.clone();
            async move {
                Listener::new(listener, metrics, rx, cancel).run().await;
            }
        });

        let (mut client1, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        let (mut client2, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.send(Utf8Bytes::from("broadcast-msg")).unwrap();

        let msg1 = client1.next().await.unwrap().unwrap();
        let msg2 = client2.next().await.unwrap().unwrap();
        assert_eq!(msg1, Message::Text(Utf8Bytes::from("broadcast-msg")));
        assert_eq!(msg2, Message::Text(Utf8Bytes::from("broadcast-msg")));

        assert_eq!(metrics.opened.load(Ordering::Relaxed), 2);

        cancel.cancel();
        let _ = handle.await;
    }
}
