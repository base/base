use core::fmt::{Debug, Formatter};
use std::sync::{Arc, Mutex, RwLock};

use tokio::{net::TcpListener, sync::broadcast::Receiver};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Utf8Bytes,
        handshake::server::{Request, Response},
    },
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{BroadcastLoop, PublisherMetrics, RingBuffer};

/// Parses `block_number` and `flashblock_index` from a URL query string.
///
/// Expects `?block_number=N&flashblock_index=M` in any order. Returns `None`
/// if either parameter is absent or cannot be parsed as `u64`.
fn parse_resume(query: Option<&str>) -> Option<(u64, u64)> {
    let query = query?;
    let mut block_number = None::<u64>;
    let mut flashblock_index = None::<u64>;

    for param in query.split('&') {
        let mut parts = param.splitn(2, '=');
        match (parts.next(), parts.next()) {
            (Some("block_number"), Some(v)) => {
                block_number = v.parse().ok();
            }
            (Some("flashblock_index"), Some(v)) => {
                flashblock_index = v.parse().ok();
            }
            _ => {}
        }
    }

    Some((block_number?, flashblock_index?))
}

/// WebSocket connection listener.
///
/// Accepts incoming TCP connections, upgrades them to WebSocket, and spawns
/// a [`BroadcastLoop`] for each connected client. Parses `?block_number=N&flashblock_index=M`
/// from the HTTP upgrade request and passes the resume position to each [`BroadcastLoop`]
/// so it can replay buffered entries before forwarding live messages.
pub struct Listener {
    listener: TcpListener,
    metrics: Arc<dyn PublisherMetrics>,
    receiver: Receiver<Utf8Bytes>,
    cancel: CancellationToken,
    ring_buffer: Arc<RwLock<RingBuffer>>,
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
        ring_buffer: Arc<RwLock<RingBuffer>>,
    ) -> Self {
        Self { listener, metrics, receiver, cancel, ring_buffer }
    }

    /// Runs the listener loop, accepting connections until cancelled.
    pub async fn run(self) {
        let Self { listener, metrics, receiver, cancel, ring_buffer } = self;

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
                    let ring_buffer = Arc::clone(&ring_buffer);

                    // Capture the resume position extracted from the HTTP upgrade request.
                    let resume_pos = Arc::new(Mutex::new(None::<(u64, u64)>));
                    let resume_pos_cb = Arc::clone(&resume_pos);

                    let accept_result = accept_hdr_async(
                        connection,
                        move |req: &Request, response: Response| {
                            *resume_pos_cb.lock().unwrap() = parse_resume(req.uri().query());
                            Ok(response)
                        },
                    )
                    .await;

                    match accept_result {
                        Ok(stream) => {
                            let resume_from = resume_pos.lock().unwrap().take();

                            tokio::spawn({
                                let metrics = Arc::clone(&metrics);
                                async move {
                                    metrics.on_connection_opened();
                                    let connected_at = std::time::Instant::now();
                                    debug!(peer_addr = %peer_addr, resume = ?resume_from, "WebSocket connection established");

                                    BroadcastLoop::new(
                                        stream,
                                        Arc::clone(&metrics),
                                        cancel,
                                        receiver_clone,
                                        ring_buffer,
                                        resume_from,
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
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    use futures::StreamExt;
    use tokio::sync::broadcast;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    use crate::RingBufferEntry;

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

    fn make_ring_buffer() -> Arc<RwLock<RingBuffer>> {
        Arc::new(RwLock::new(RingBuffer::new(16)))
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
                Listener::new(listener, metrics, rx, cancel, make_ring_buffer()).run().await;
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
                Listener::new(listener, metrics, rx, cancel, make_ring_buffer()).run().await;
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
                Listener::new(listener, metrics, rx, cancel, make_ring_buffer()).run().await;
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

    #[test]
    fn parse_resume_valid() {
        assert_eq!(
            parse_resume(Some("block_number=10&flashblock_index=3")),
            Some((10, 3))
        );
        assert_eq!(
            parse_resume(Some("flashblock_index=5&block_number=20")),
            Some((20, 5))
        );
    }

    #[test]
    fn parse_resume_missing_params() {
        assert_eq!(parse_resume(None), None);
        assert_eq!(parse_resume(Some("block_number=10")), None);
        assert_eq!(parse_resume(Some("flashblock_index=3")), None);
        assert_eq!(parse_resume(Some("")), None);
    }

    #[tokio::test]
    async fn listener_with_resume_replays_buffered() {
        let (listener, addr) = bind_listener().await;
        let (tx, rx) = broadcast::channel::<Utf8Bytes>(16);
        let cancel = CancellationToken::new();
        let metrics: Arc<dyn PublisherMetrics> = Arc::new(MockMetrics::new());
        let ring_buffer = make_ring_buffer();

        // Pre-populate ring buffer with two entries
        {
            let mut buf = ring_buffer.write().unwrap();
            buf.push(RingBufferEntry {
                block_number: 1,
                flashblock_index: 0,
                payload: Utf8Bytes::from("entry-1-0"),
            });
            buf.push(RingBufferEntry {
                block_number: 1,
                flashblock_index: 1,
                payload: Utf8Bytes::from("entry-1-1"),
            });
        }

        let ring_buffer_clone = Arc::clone(&ring_buffer);
        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                Listener::new(listener, metrics, rx, cancel, ring_buffer_clone).run().await;
            }
        });

        // Connect with resume position asking for entries after (1, 0).
        // Note: path must be explicit (`/`) to produce a valid HTTP request line.
        let (mut client, _) =
            connect_async(format!("ws://{addr}/?block_number=1&flashblock_index=0"))
                .await
                .unwrap();

        // Should receive the buffered entry (1, 1)
        let msg = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            client.next(),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(msg, Message::Text(Utf8Bytes::from("entry-1-1")));

        // Then live messages still work
        tx.send(Utf8Bytes::from("live-msg")).unwrap();
        let live_msg = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            client.next(),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(live_msg, Message::Text(Utf8Bytes::from("live-msg")));

        cancel.cancel();
        let _ = handle.await;
    }
}
