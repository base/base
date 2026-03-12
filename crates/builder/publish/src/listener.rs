use core::fmt::{Debug, Formatter};
use std::sync::Arc;

use base_ring_buffer::RingBuffer;
use parking_lot::RwLock;
use tokio::{net::TcpListener, sync::broadcast::Receiver, time::timeout};
use tokio_tungstenite::{accept_hdr_async, tungstenite::Utf8Bytes};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{BroadcastLoop, FlashblockPosition, PositionedPayload, PublisherMetrics};

/// Timeout for the WebSocket handshake.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// WebSocket connection listener.
///
/// Accepts incoming TCP connections, upgrades them to WebSocket, and spawns
/// a [`BroadcastLoop`] for each connected client.
pub struct Listener {
    listener: TcpListener,
    metrics: Arc<dyn PublisherMetrics>,
    receiver: Receiver<PositionedPayload>,
    ring_buffer: Arc<RwLock<RingBuffer<FlashblockPosition, Utf8Bytes>>>,
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
        receiver: Receiver<PositionedPayload>,
        ring_buffer: Arc<RwLock<RingBuffer<FlashblockPosition, Utf8Bytes>>>,
        cancel: CancellationToken,
    ) -> Self {
        Self { listener, metrics, receiver, ring_buffer, cancel }
    }

    /// Runs the listener loop, accepting connections until cancelled.
    pub async fn run(self) {
        let Self { listener, metrics, receiver, ring_buffer, cancel } = self;

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
                    let receiver = receiver.resubscribe();
                    let ring_buffer = Arc::clone(&ring_buffer);
                    let metrics = Arc::clone(&metrics);

                    let (pos_tx, pos_rx) = tokio::sync::oneshot::channel();
                    let handshake = accept_hdr_async(connection, move |req: &http::Request<()>, resp| {
                        let _ = pos_tx.send(parse_resume_position(req));
                        Ok(resp)
                    });

                    match timeout(HANDSHAKE_TIMEOUT, handshake).await {
                        Ok(Ok(stream)) => {
                            let resume_from = pos_rx.await.ok().flatten();
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
                                        receiver,
                                        Arc::clone(&ring_buffer),
                                        resume_from,
                                    )
                                    .run()
                                    .await;

                                    metrics.on_connection_closed(connected_at.elapsed());
                                    debug!(peer_addr = %peer_addr, "WebSocket connection closed");
                                }
                            });
                        }
                        Ok(Err(e)) => {
                            metrics.on_handshake_error();
                            warn!(peer_addr = %peer_addr, error = %e, "Failed to accept WebSocket connection");
                        }
                        Err(_) => {
                            metrics.on_handshake_error();
                            warn!(peer_addr = %peer_addr, "WebSocket handshake timed out");
                        }
                    }
                }
            }
        }
    }
}

/// Parses `block_number` and `flashblock_index` query parameters from a
/// WebSocket upgrade request URI.
fn parse_resume_position(request: &http::Request<()>) -> Option<FlashblockPosition> {
    let query = request.uri().query()?;
    let mut block_number = None;
    let mut flashblock_index = None;

    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "block_number" => block_number = value.parse().ok(),
                "flashblock_index" => flashblock_index = value.parse().ok(),
                _ => {}
            }
        }
    }

    Some((block_number?, flashblock_index?))
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    use futures::StreamExt;
    use tokio::sync::broadcast;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    use super::*;

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
        let (tx, rx) = broadcast::channel::<PositionedPayload>(16);
        let cancel = CancellationToken::new();
        let metrics = Arc::new(MockMetrics::new());
        let ring_buffer = Arc::new(RwLock::new(RingBuffer::new(16)));

        let handle = tokio::spawn({
            let metrics = Arc::clone(&metrics) as Arc<dyn PublisherMetrics>;
            let cancel = cancel.clone();
            async move {
                Listener::new(listener, metrics, rx, ring_buffer, cancel).run().await;
            }
        });

        let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.send(((1, 0), Utf8Bytes::from("test-payload"))).unwrap();

        let msg = client.next().await.unwrap().unwrap();
        assert_eq!(msg, Message::Text(Utf8Bytes::from("test-payload")));

        assert_eq!(metrics.opened.load(Ordering::Relaxed), 1);

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn listener_graceful_shutdown() {
        let (listener, _addr) = bind_listener().await;
        let (_, rx) = broadcast::channel::<PositionedPayload>(16);
        let cancel = CancellationToken::new();
        let metrics: Arc<dyn PublisherMetrics> = Arc::new(MockMetrics::new());
        let ring_buffer = Arc::new(RwLock::new(RingBuffer::new(16)));

        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                Listener::new(listener, metrics, rx, ring_buffer, cancel).run().await;
            }
        });

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn listener_multiple_clients_receive_same_message() {
        let (listener, addr) = bind_listener().await;
        let (tx, rx) = broadcast::channel::<PositionedPayload>(16);
        let cancel = CancellationToken::new();
        let metrics = Arc::new(MockMetrics::new());
        let ring_buffer = Arc::new(RwLock::new(RingBuffer::new(16)));

        let handle = tokio::spawn({
            let metrics = Arc::clone(&metrics) as Arc<dyn PublisherMetrics>;
            let cancel = cancel.clone();
            async move {
                Listener::new(listener, metrics, rx, ring_buffer, cancel).run().await;
            }
        });

        let (mut client1, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        let (mut client2, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.send(((1, 0), Utf8Bytes::from("broadcast-msg"))).unwrap();

        let msg1 = client1.next().await.unwrap().unwrap();
        let msg2 = client2.next().await.unwrap().unwrap();
        assert_eq!(msg1, Message::Text(Utf8Bytes::from("broadcast-msg")));
        assert_eq!(msg2, Message::Text(Utf8Bytes::from("broadcast-msg")));

        assert_eq!(metrics.opened.load(Ordering::Relaxed), 2);

        cancel.cancel();
        let _ = handle.await;
    }

    #[test]
    fn parse_resume_both_params() {
        let req = http::Request::builder()
            .uri("ws://localhost/ws?block_number=100&flashblock_index=3")
            .body(())
            .unwrap();
        assert_eq!(parse_resume_position(&req), Some((100, 3)));
    }

    #[test]
    fn parse_resume_missing_flashblock_index() {
        let req =
            http::Request::builder().uri("ws://localhost/ws?block_number=100").body(()).unwrap();
        assert_eq!(parse_resume_position(&req), None);
    }

    #[test]
    fn parse_resume_missing_block_number() {
        let req =
            http::Request::builder().uri("ws://localhost/ws?flashblock_index=3").body(()).unwrap();
        assert_eq!(parse_resume_position(&req), None);
    }

    #[test]
    fn parse_resume_no_query() {
        let req = http::Request::builder().uri("ws://localhost/ws").body(()).unwrap();
        assert_eq!(parse_resume_position(&req), None);
    }

    #[test]
    fn parse_resume_extra_params_ignored() {
        let req = http::Request::builder()
            .uri("ws://localhost/ws?token=abc&block_number=42&flashblock_index=0&extra=1")
            .body(())
            .unwrap();
        assert_eq!(parse_resume_position(&req), Some((42, 0)));
    }

    #[tokio::test]
    async fn listener_resume_replays_from_ring_buffer() {
        let (listener, addr) = bind_listener().await;
        let (_tx, rx) = broadcast::channel::<PositionedPayload>(16);
        let cancel = CancellationToken::new();
        let metrics = Arc::new(MockMetrics::new());
        let ring_buffer = Arc::new(RwLock::new(RingBuffer::new(16)));

        // Pre-populate the ring buffer with entries.
        {
            let mut buf = ring_buffer.write();
            buf.push(Some((100, 0)), Utf8Bytes::from("msg-100-0"));
            buf.push(Some((100, 1)), Utf8Bytes::from("msg-100-1"));
            buf.push(Some((101, 0)), Utf8Bytes::from("msg-101-0"));
        }

        let handle = tokio::spawn({
            let metrics = Arc::clone(&metrics) as Arc<dyn PublisherMetrics>;
            let cancel = cancel.clone();
            let ring_buffer = Arc::clone(&ring_buffer);
            async move {
                Listener::new(listener, metrics, rx, ring_buffer, cancel).run().await;
            }
        });

        // Connect with resume position — should replay entries after (100, 0).
        let (mut client, _) =
            connect_async(format!("ws://{addr}/?block_number=100&flashblock_index=0"))
                .await
                .unwrap();

        let msg1 = client.next().await.unwrap().unwrap();
        assert_eq!(msg1, Message::Text(Utf8Bytes::from("msg-100-1")));

        let msg2 = client.next().await.unwrap().unwrap();
        assert_eq!(msg2, Message::Text(Utf8Bytes::from("msg-101-0")));

        cancel.cancel();
        let _ = handle.await;
    }
}
