use core::fmt::{Debug, Formatter};
use std::sync::{Arc, RwLock};

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

use crate::{PublisherMetrics, RingBuffer};

/// Per-client broadcast sender.
///
/// Created for each connected WebSocket client. If `resume_from` is set, replays
/// all ring-buffered entries after that position before forwarding live messages.
/// Exits on cancellation, channel close, or WebSocket error.
pub struct BroadcastLoop {
    stream: WebSocketStream<TcpStream>,
    metrics: Arc<dyn PublisherMetrics>,
    cancel: CancellationToken,
    blocks: broadcast::Receiver<Utf8Bytes>,
    ring_buffer: Arc<RwLock<RingBuffer>>,
    resume_from: Option<(u64, u64)>,
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
        ring_buffer: Arc<RwLock<RingBuffer>>,
        resume_from: Option<(u64, u64)>,
    ) -> Self {
        Self { stream, metrics, cancel, blocks, ring_buffer, resume_from }
    }

    /// Runs the broadcast loop until cancellation or error.
    ///
    /// If `resume_from` is set, buffered entries after that position are sent first,
    /// then the loop joins the live broadcast channel. Brief overlap between replayed
    /// and live messages is acceptable; clients deduplicate by `(payload_id, index)`.
    pub async fn run(mut self) {
        let Ok(peer_addr) = self.stream.get_ref().peer_addr() else {
            return;
        };

        // Replay buffered entries before entering the live loop.
        if let Some((bn, fi)) = self.resume_from {
            let entries = self.ring_buffer.read().unwrap().entries_after(bn, fi);
            debug!(
                peer_addr = %peer_addr,
                block_number = bn,
                flashblock_index = fi,
                count = entries.len(),
                "Replaying ring buffer entries"
            );
            for payload in entries {
                self.metrics.on_message_sent();
                if let Err(e) = self.stream.send(Message::Text(payload)).await {
                    self.metrics.on_send_error();
                    debug!(peer_addr = %peer_addr, error = %e, "Error during ring buffer replay");
                    return;
                }
            }
        }

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
    use std::sync::{
        RwLock,
        atomic::{AtomicU64, Ordering},
    };

    use rstest::rstest;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, connect_async};

    use super::*;
    use crate::{RingBuffer, RingBufferEntry};

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

    fn empty_ring_buffer() -> Arc<RwLock<RingBuffer>> {
        Arc::new(RwLock::new(RingBuffer::new(16)))
    }

    fn ring_buffer_with_entries(entries: Vec<(u64, u64, &str)>) -> Arc<RwLock<RingBuffer>> {
        let buf = Arc::new(RwLock::new(RingBuffer::new(16)));
        let mut locked = buf.write().unwrap();
        for (bn, fi, payload) in entries {
            locked.push(RingBufferEntry {
                block_number: bn,
                flashblock_index: fi,
                payload: Utf8Bytes::from(payload),
            });
        }
        drop(locked);
        buf
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
                BroadcastLoop::new(ws, metrics, cancel, rx, empty_ring_buffer(), None).run().await;
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
                BroadcastLoop::new(ws, metrics, cancel, rx, empty_ring_buffer(), None).run().await;
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
            BroadcastLoop::new(ws, metrics, cancel, rx, empty_ring_buffer(), None).run().await;
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
                BroadcastLoop::new(ws, metrics_clone, cancel, rx, empty_ring_buffer(), None)
                    .run()
                    .await;
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

    #[tokio::test]
    async fn broadcast_loop_replays_ring_buffer_before_live() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = broadcast::channel::<Utf8Bytes>(16);
        let cancel = CancellationToken::new();
        let metrics: Arc<dyn PublisherMetrics> = Arc::new(MockMetrics::new());

        let ring_buffer = ring_buffer_with_entries(vec![
            (1, 0, "entry-1-0"),
            (1, 1, "entry-1-1"),
            (2, 0, "entry-2-0"),
        ]);

        let server_handle = tokio::spawn({
            let metrics = Arc::clone(&metrics);
            let cancel = cancel.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let ws = accept_async(stream).await.unwrap();
                // Resume from (1, 0) — should get (1,1) and (2,0)
                BroadcastLoop::new(ws, metrics, cancel, rx, ring_buffer, Some((1, 0))).run().await;
            }
        });

        let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        // First two messages are replayed from ring buffer
        let msg1 = client.next().await.unwrap().unwrap();
        assert_eq!(msg1, Message::Text(Utf8Bytes::from("entry-1-1")));

        let msg2 = client.next().await.unwrap().unwrap();
        assert_eq!(msg2, Message::Text(Utf8Bytes::from("entry-2-0")));

        // Live messages follow
        tx.send(Utf8Bytes::from("live")).unwrap();
        let msg3 = client.next().await.unwrap().unwrap();
        assert_eq!(msg3, Message::Text(Utf8Bytes::from("live")));

        cancel.cancel();
        let _ = server_handle.await;
    }
}
