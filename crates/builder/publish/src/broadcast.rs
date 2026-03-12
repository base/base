use core::fmt::{Debug, Formatter};
use std::{collections::HashSet, sync::Arc, time::Duration};

use base_ring_buffer::RingBuffer;
use futures::{SinkExt, StreamExt};
use parking_lot::RwLock;
use tokio::{
    net::TcpStream,
    sync::broadcast::{self, Receiver, error::RecvError},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, Utf8Bytes},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{FlashblockPosition, PositionedPayload, PublisherMetrics};

/// Timeout for sending a single replay message during the replay phase.
const REPLAY_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-client broadcast loop.
///
/// Created for each connected WebSocket client. When `resume_from` is set,
/// replays missed entries from the ring buffer (deduplicating against messages
/// already accumulated on the receiver) before switching to live broadcast.
pub struct BroadcastLoop {
    stream: WebSocketStream<TcpStream>,
    metrics: Arc<dyn PublisherMetrics>,
    cancel: CancellationToken,
    receiver: Receiver<PositionedPayload>,
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
        receiver: Receiver<PositionedPayload>,
        ring_buffer: Arc<RwLock<RingBuffer<FlashblockPosition, Utf8Bytes>>>,
        resume_from: Option<FlashblockPosition>,
    ) -> Self {
        Self { stream, metrics, cancel, receiver, ring_buffer, resume_from }
    }

    /// Runs the broadcast loop until cancellation or error.
    pub async fn run(mut self) {
        let Ok(peer_addr) = self.stream.get_ref().peer_addr() else {
            return;
        };

        // If resume_from is set, replay from ring buffer first.
        if let Some(cutoff) = self.resume_from.take()
            && let Err(e) = self.replay(&cutoff).await
        {
            debug!(peer_addr = %peer_addr, error = %e, "Replay failed, closing connection");
            return;
        }

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("WebSocketPublisher is terminating, closing broadcast loop");
                    return;
                }

                payload = self.receiver.recv() => match payload {
                    Ok((_position, data)) => {
                        self.metrics.on_message_sent();

                        debug!(payload = ?data, "Broadcasted payload");
                        if let Err(e) = self.stream.send(Message::Text(data)).await {
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

    /// Two-phase replay for reconnecting clients.
    ///
    /// The receiver was subscribed at connection accept time, so it has been
    /// accumulating live messages since then.
    ///
    /// - **Phase 1**: Snapshot the ring buffer and send all entries after
    ///   `cutoff`, tracking sent positions for dedup.
    /// - **Phase 2**: Drain any messages that accumulated on the receiver
    ///   during phase 1, deduplicating by position.
    async fn replay(&mut self, cutoff: &FlashblockPosition) -> Result<(), ReplayError> {
        // Phase 1: snapshot ring buffer and replay.
        let snapshot: Vec<_> = {
            let buf = self.ring_buffer.read();
            buf.positioned_entries_after(cutoff)
                .map(|(pos, val)| (pos.copied(), val.clone()))
                .collect()
        };
        let mut sent_positions = HashSet::with_capacity(snapshot.len());
        for (pos, val) in snapshot {
            if let Some(p) = pos {
                sent_positions.insert(p);
            }
            self.send_replay_message(val).await?;
        }

        // Phase 2: drain receiver messages that accumulated during phase 1.
        loop {
            match self.receiver.try_recv() {
                Ok((pos, data)) => {
                    if !sent_positions.insert(pos) {
                        continue;
                    }
                    self.send_replay_message(data).await?;
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => {
                    return Err(ReplayError::ChannelClosed);
                }
            }
        }

        Ok(())
    }

    /// Sends a single message during the replay phase with a timeout.
    async fn send_replay_message(&mut self, data: Utf8Bytes) -> Result<(), ReplayError> {
        tokio::time::timeout(REPLAY_SEND_TIMEOUT, self.stream.send(Message::Text(data)))
            .await
            .map_err(|_| ReplayError::SendTimeout)?
            .map_err(ReplayError::WebSocket)?;
        self.metrics.on_message_sent();
        Ok(())
    }
}

/// Errors that can occur during the replay phase.
#[derive(Debug)]
enum ReplayError {
    /// A replay message send timed out.
    SendTimeout,
    /// The broadcast channel was closed during replay.
    ChannelClosed,
    /// A WebSocket send error occurred.
    WebSocket(tokio_tungstenite::tungstenite::Error),
}

impl core::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SendTimeout => write!(f, "replay send timed out"),
            Self::ChannelClosed => write!(f, "broadcast channel closed during replay"),
            Self::WebSocket(e) => write!(f, "websocket error during replay: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

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
        let (tx, rx) = broadcast::channel::<PositionedPayload>(16);
        let cancel = CancellationToken::new();
        let metrics: Arc<dyn crate::PublisherMetrics> = Arc::new(MockMetrics::new());
        let ring_buffer = Arc::new(RwLock::new(RingBuffer::new(16)));

        let server_handle = tokio::spawn({
            let metrics = Arc::clone(&metrics);
            let cancel = cancel.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let ws = accept_async(stream).await.unwrap();
                BroadcastLoop::new(ws, metrics, cancel, rx, ring_buffer, None).run().await;
            }
        });

        let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        tx.send(((1, 0), Utf8Bytes::from("hello"))).unwrap();

        let msg = client.next().await.unwrap().unwrap();
        assert_eq!(msg, Message::Text(Utf8Bytes::from("hello")));

        cancel.cancel();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn broadcast_loop_exits_on_cancellation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_tx, rx) = broadcast::channel::<PositionedPayload>(16);
        let cancel = CancellationToken::new();
        let metrics: Arc<dyn crate::PublisherMetrics> = Arc::new(MockMetrics::new());
        let ring_buffer = Arc::new(RwLock::new(RingBuffer::new(16)));

        let server_handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let ws = accept_async(stream).await.unwrap();
                BroadcastLoop::new(ws, metrics, cancel, rx, ring_buffer, None).run().await;
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
        let metrics: Arc<dyn crate::PublisherMetrics> = Arc::new(MockMetrics::new());
        let ring_buffer = Arc::new(RwLock::new(RingBuffer::new(16)));

        let server_handle = tokio::spawn({
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let ws = accept_async(stream).await.unwrap();
                BroadcastLoop::new(ws, metrics, cancel, rx, ring_buffer, None).run().await;
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

        let (tx, rx) = broadcast::channel::<PositionedPayload>(1);
        let cancel = CancellationToken::new();
        let metrics = Arc::new(MockMetrics::new());
        let metrics_clone: Arc<dyn crate::PublisherMetrics> =
            Arc::clone(&metrics) as Arc<dyn crate::PublisherMetrics>;
        let ring_buffer = Arc::new(RwLock::new(RingBuffer::new(16)));

        let server_handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let ws = accept_async(stream).await.unwrap();
                BroadcastLoop::new(ws, metrics_clone, cancel, rx, ring_buffer, None).run().await;
            }
        });

        let (_client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        for i in 0..5 {
            let _ = tx.send(((1, i), Utf8Bytes::from(format!("msg{i}"))));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(metrics.lagged.load(Ordering::Relaxed) > 0);
        cancel.cancel();

        server_handle.await.unwrap();
    }
}
