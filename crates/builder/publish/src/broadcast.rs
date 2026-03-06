use core::fmt::{Debug, Formatter};
use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
    time::Duration,
};

use base_ring_buffer::RingBuffer;
use futures::{SinkExt, StreamExt};
use tokio::{
    net::TcpStream,
    sync::broadcast::{
        self,
        error::{RecvError, TryRecvError},
    },
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, Utf8Bytes},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{FlashblockPosition, PositionedPayload, PublisherMetrics};

/// Maximum time to wait for a WebSocket send during the gap-fill drain phase.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-client broadcast sender.
///
/// Created for each connected WebSocket client. If `resume_from` is set, replays
/// all ring-buffered entries after that position before forwarding live messages.
/// Exits on cancellation, channel close, or WebSocket error.
pub struct BroadcastLoop {
    stream: WebSocketStream<TcpStream>,
    metrics: Arc<dyn PublisherMetrics>,
    cancel: CancellationToken,
    blocks: broadcast::Receiver<PositionedPayload>,
    ring_buffer: Arc<RwLock<RingBuffer<FlashblockPosition, Utf8Bytes>>>,
    resume_from: Option<FlashblockPosition>,
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
        blocks: broadcast::Receiver<PositionedPayload>,
        ring_buffer: Arc<RwLock<RingBuffer<FlashblockPosition, Utf8Bytes>>>,
        resume_from: Option<FlashblockPosition>,
    ) -> Self {
        Self { stream, metrics, cancel, blocks, ring_buffer, resume_from }
    }

    /// Runs the broadcast loop until cancellation or error.
    ///
    /// If `resume_from` is set, buffered entries after that position are sent first.
    /// After replay the receiver is drained: entries whose position matches one already
    /// sent are skipped (duplicates), while entries with a new position are forwarded
    /// (gap-fill). This avoids both duplicate delivery and a gap in coverage.
    pub async fn run(mut self) {
        let Ok(peer_addr) = self.stream.get_ref().peer_addr() else {
            return;
        };

        // Replay buffered entries before entering the live loop.
        if let Some((bn, fi)) = self.resume_from {
            let entries: Vec<(Option<FlashblockPosition>, Utf8Bytes)> = self
                .ring_buffer
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .positioned_entries_after(&(bn, fi))
                .map(|(pos, payload)| (pos.copied(), payload.clone()))
                .collect();
            debug!(
                peer_addr = %peer_addr,
                block_number = %bn,
                flashblock_index = %fi,
                count = entries.len(),
                "Replaying ring buffer entries"
            );
            for (_, payload) in &entries {
                if self.cancel.is_cancelled() {
                    return;
                }
                self.metrics.on_message_sent();
                match tokio::time::timeout(
                    SEND_TIMEOUT,
                    self.stream.send(Message::Text(payload.clone())),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        self.metrics.on_send_error();
                        debug!(peer_addr = %peer_addr, error = %e, "Error during ring buffer replay");
                        return;
                    }
                    Err(_) => {
                        self.metrics.on_send_error();
                        debug!(peer_addr = %peer_addr, "Send timeout during ring buffer replay");
                        return;
                    }
                }
            }

            // Drain messages that accumulated in the receiver during replay.
            // Position-based deduplication is used instead of byte equality: it is
            // cheaper (16 bytes per entry vs full payload clones) and semantically
            // correct (identical content at different positions is not suppressed).
            let replayed_positions: HashSet<FlashblockPosition> =
                entries.into_iter().filter_map(|(pos, _)| pos).collect();
            loop {
                match self.blocks.try_recv() {
                    Ok((pos, payload)) => {
                        if pos.is_some_and(|p| replayed_positions.contains(&p)) {
                            continue;
                        }
                        if self.cancel.is_cancelled() {
                            return;
                        }
                        self.metrics.on_message_sent();
                        match tokio::time::timeout(
                            SEND_TIMEOUT,
                            self.stream.send(Message::Text(payload)),
                        )
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                self.metrics.on_send_error();
                                debug!(peer_addr = %peer_addr, error = %e, "Error during replay drain");
                                return;
                            }
                            Err(_) => {
                                self.metrics.on_send_error();
                                debug!(peer_addr = %peer_addr, "Send timeout during replay drain");
                                return;
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Lagged(n)) => {
                        self.metrics.on_lagged(n);
                        warn!(skipped = n, "Broadcast channel lagged during replay drain");
                        self.blocks = self.blocks.resubscribe();
                        break;
                    }
                    Err(TryRecvError::Closed) => return,
                }
            }
        }

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("WebSocketPublisher is terminating, closing broadcast loop");
                    return;
                }

                result = self.blocks.recv() => match result {
                    Ok((_, payload)) => {
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

    use base_ring_buffer::RingBuffer;
    use rstest::rstest;
    use tokio::net::TcpListener;
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

    fn empty_ring_buffer() -> Arc<RwLock<RingBuffer<FlashblockPosition, Utf8Bytes>>> {
        Arc::new(RwLock::new(RingBuffer::new(16)))
    }

    fn ring_buffer_with_entries(
        entries: Vec<(u64, u64, &str)>,
    ) -> Arc<RwLock<RingBuffer<FlashblockPosition, Utf8Bytes>>> {
        let buf = Arc::new(RwLock::new(RingBuffer::new(16)));
        let mut locked = buf.write().unwrap();
        for (bn, fi, payload) in entries {
            locked.push(Some((bn, fi)), Utf8Bytes::from(payload));
        }
        drop(locked);
        buf
    }

    #[tokio::test]
    async fn broadcast_loop_forwards_messages() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = broadcast::channel::<PositionedPayload>(16);
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

        tx.send((Some((1, 0)), Utf8Bytes::from("hello"))).unwrap();

        let msg = client.next().await.unwrap().unwrap();
        assert_eq!(msg, Message::Text(Utf8Bytes::from("hello")));

        cancel.cancel();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn broadcast_loop_exits_on_cancellation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_, rx) = broadcast::channel::<PositionedPayload>(16);
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
        let (_tx, rx) = broadcast::channel::<PositionedPayload>(16);
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

        let (tx, rx) = broadcast::channel::<PositionedPayload>(1);
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
                let _ = tx.send((Some((1, i)), Utf8Bytes::from(format!("msg{i}"))));
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
        let (tx, rx) = broadcast::channel::<PositionedPayload>(16);
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
        tx.send((Some((3, 0)), Utf8Bytes::from("live"))).unwrap();
        let msg3 = client.next().await.unwrap().unwrap();
        assert_eq!(msg3, Message::Text(Utf8Bytes::from("live")));

        cancel.cancel();
        let _ = server_handle.await;
    }
}
