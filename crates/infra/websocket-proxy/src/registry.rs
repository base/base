use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
    time::Instant,
};

use axum::extract::ws::Message;
use bytes::Bytes;
use futures::{SinkExt, stream::StreamExt};
use tokio::{
    sync::broadcast::{
        Sender,
        error::{RecvError, TryRecvError},
    },
    time::{Duration, interval, timeout},
};
use tracing::{debug, info, trace, warn};

use crate::{
    FlashblockPosition, FlashblocksRingBuffer, PositionedMessage, client::ClientConnection,
    metrics::Metrics,
};

fn get_message_size(msg: &Message) -> u64 {
    match msg {
        Message::Text(text) => text.len() as u64,
        Message::Binary(data) | Message::Ping(data) | Message::Pong(data) => data.len() as u64,
        Message::Close(_) => 0,
    }
}

/// Manages broadcast subscriptions for connected WebSocket clients.
#[derive(Clone, Debug)]
pub struct Registry {
    sender: Sender<PositionedMessage>,
    metrics: Arc<Metrics>,
    compressed: bool,
    ping_enabled: bool,
    pong_timeout_ms: u64,
    send_timeout_ms: Duration,
    ring_buffer: Arc<RwLock<FlashblocksRingBuffer>>,
}

impl Registry {
    /// Creates a new registry with the given broadcast sender, configuration, and ring buffer.
    pub const fn new(
        sender: Sender<PositionedMessage>,
        metrics: Arc<Metrics>,
        compressed: bool,
        ping_enabled: bool,
        pong_timeout_ms: u64,
        send_timeout_ms: Duration,
        ring_buffer: Arc<RwLock<FlashblocksRingBuffer>>,
    ) -> Self {
        Self {
            sender,
            metrics,
            compressed,
            ping_enabled,
            pong_timeout_ms,
            send_timeout_ms,
            ring_buffer,
        }
    }

    /// Subscribes a client to the broadcast channel and forwards matching messages.
    ///
    /// If the client has a `resume_position`, buffered entries after that position are sent
    /// first. After replay the receiver is drained: entries whose position matches one
    /// already sent are skipped (duplicates), while entries with a new position are
    /// forwarded (gap-fill). This avoids both duplicate delivery and a gap in coverage.
    pub async fn subscribe(&self, client: ClientConnection) {
        info!(message = "subscribing client", client = client.id());

        // Subscribe to the live channel *before* snapshotting the ring buffer so that
        // messages arriving between the snapshot and the start of the live loop are
        // captured in the receiver. The drain phase deduplicates entries that appear in
        // both the snapshot and the receiver. Tradeoff: if the broadcast channel capacity
        // is exceeded during a long replay the receiver will lag, and the resubscribe
        // fallback loses the messages between the lag point and the new subscription.
        let mut receiver = self.sender.subscribe();
        let metrics = Arc::clone(&self.metrics);
        metrics.new_connections.increment(1);

        let filter = client.filter.clone();
        let compressed = self.compressed;
        let client_id = client.id();
        let resume_position = client.resume_position;
        let (mut ws_sender, ws_receiver) = client.websocket.split();

        let (pong_error_tx, mut pong_error_rx) = tokio::sync::oneshot::channel();
        let client_reader = self.start_reader(ws_receiver, client_id.clone(), pong_error_tx);

        // Replay ring buffer entries before joining the live stream.
        if let Some((bn, fi)) = resume_position {
            let entries: Vec<(Option<FlashblockPosition>, Bytes)> = self
                .ring_buffer
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .positioned_entries_after(&(bn, fi))
                .map(|(pos, payload)| (pos.copied(), payload.clone()))
                .collect();
            debug!(
                client = %client_id,
                block_number = %bn,
                flashblock_index = %fi,
                count = entries.len(),
                "Replaying ring buffer entries to client"
            );
            for (_, payload) in &entries {
                if !filter.matches(payload, compressed) {
                    continue;
                }
                let msg = Message::Binary(payload.clone());
                let send_result = timeout(self.send_timeout_ms, ws_sender.send(msg)).await;
                match send_result {
                    Ok(Ok(())) => {
                        metrics.sent_messages.increment(1);
                    }
                    Ok(Err(e)) => {
                        warn!(
                            client = %client_id,
                            error = %e,
                            "Failed to send ring buffer entry to client"
                        );
                        client_reader.abort();
                        metrics.closed_connections.increment(1);
                        return;
                    }
                    Err(_) => {
                        warn!(client = %client_id, "Send timeout during ring buffer replay");
                        client_reader.abort();
                        metrics.closed_connections.increment(1);
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
                match receiver.try_recv() {
                    Ok((pos, msg)) => {
                        if pos.is_some_and(|p| replayed_positions.contains(&p)) {
                            continue;
                        }
                        let msg_bytes: &[u8] = match &msg {
                            Message::Binary(data) => data.as_ref(),
                            _ => &[],
                        };
                        if filter.matches(msg_bytes, compressed) {
                            let send_result =
                                timeout(self.send_timeout_ms, ws_sender.send(msg)).await;
                            match send_result {
                                Ok(Ok(())) => metrics.sent_messages.increment(1),
                                Ok(Err(e)) => {
                                    warn!(
                                        client = %client_id,
                                        error = %e,
                                        "Failed to send gap-fill entry to client"
                                    );
                                    client_reader.abort();
                                    metrics.closed_connections.increment(1);
                                    return;
                                }
                                Err(_) => {
                                    warn!(
                                        client = %client_id,
                                        "Send timeout during replay drain"
                                    );
                                    client_reader.abort();
                                    metrics.closed_connections.increment(1);
                                    return;
                                }
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Lagged(_)) => {
                        metrics.lagged_connections.increment(1);
                        receiver = self.sender.subscribe();
                        break;
                    }
                    Err(TryRecvError::Closed) => break,
                }
            }
        }

        loop {
            tokio::select! {
                broadcast_result = receiver.recv() => {
                    match broadcast_result {
                        Ok((_, msg)) => {
                            let msg_bytes = match &msg {
                                Message::Binary(data) => data.as_ref(),
                                _ => &[],
                            };
                            if filter.matches(msg_bytes, compressed) {
                                trace!(message = "filter matched for client", client = client_id, filter = ?filter);

                                let send_start = Instant::now();
                                let msg_size = get_message_size(&msg);
                                let send_result = timeout(self.send_timeout_ms, ws_sender.send(msg)).await;
                                let send_duration = send_start.elapsed();

                                metrics.message_send_duration.record(send_duration);

                                match send_result {
                                    Ok(Ok(())) => {
                                        // Success - message sent
                                        trace!(message = "message sent to client", client = client_id);
                                        metrics.sent_messages.increment(1);
                                        metrics.bytes_broadcasted.increment(msg_size);
                                    }
                                    Ok(Err(e)) => {
                                        // Send failed (connection error)
                                        warn!(
                                            message = "failed to send data to client",
                                            client = client_id,
                                            error = e.to_string()
                                        );
                                        metrics.failed_messages.increment(1);
                                        break;
                                    }
                                    Err(_) => {
                                        // Timeout - client too slow
                                        warn!(
                                            message = "send timeout - disconnecting slow client",
                                            client = client_id,
                                            timeout_ms = self.send_timeout_ms.as_millis()
                                        );
                                        metrics.failed_messages.increment(1);
                                        break;
                                    }
                                }
                            } else {
                                trace!(client_id = %client_id, "Filter did not match");
                            }
                        }
                        Err(RecvError::Closed) => {
                            info!(message = "upstream connection closed", client = client_id);
                            break;
                        }
                        Err(RecvError::Lagged(_)) => {
                            info!(message = "client is lagging", client = client_id);
                            metrics.lagged_connections.increment(1);
                            break;
                        }
                    }
                }

                _ = &mut pong_error_rx => {
                    debug!(message = "client reader signaled disconnect", client = client_id);
                    break;
                }
            }
        }

        client_reader.abort();
        metrics.closed_connections.increment(1);

        info!(message = "client disconnected", client = client_id);
    }

    fn start_reader(
        &self,
        ws_receiver: futures::stream::SplitStream<axum::extract::ws::WebSocket>,
        client_id: String,
        pong_error_tx: tokio::sync::oneshot::Sender<()>,
    ) -> tokio::task::JoinHandle<()> {
        let ping_enabled = self.ping_enabled;
        let pong_timeout_ms = self.pong_timeout_ms;
        let metrics = Arc::clone(&self.metrics);

        tokio::spawn(async move {
            let mut ws_receiver = ws_receiver;
            let mut last_pong = Instant::now();
            let mut timeout_checker = interval(Duration::from_millis(pong_timeout_ms / 4));
            let pong_timeout = Duration::from_millis(pong_timeout_ms);

            loop {
                tokio::select! {
                    msg = ws_receiver.next() => {
                        match msg {
                            Some(Ok(Message::Pong(_))) => {
                                if ping_enabled {
                                    trace!(message = "received pong from client", client = client_id);
                                    last_pong = Instant::now();
                                }
                            }
                            Some(Ok(Message::Close(_))) => {
                                trace!(message = "received close from client", client = client_id);
                                let _ = pong_error_tx.send(());
                                return;
                            }
                            Some(Err(e)) => {
                                trace!(
                                    message = "error receiving from client",
                                    client = client_id,
                                    error = e.to_string()
                                );
                                let _ = pong_error_tx.send(());
                                return;
                            }
                            None => {
                                trace!(message = "client connection closed", client = client_id);
                                let _ = pong_error_tx.send(());
                                return;
                            }
                            _ => {}
                        }
                    }

                    _ = timeout_checker.tick() => {
                        if ping_enabled && last_pong.elapsed() > pong_timeout  {
                            debug!(
                                message = "client pong timeout, disconnecting",
                                client = client_id,
                                elapsed_ms = last_pong.elapsed().as_millis()
                            );
                            metrics.client_pong_disconnects.increment(1);
                            let _ = pong_error_tx.send(());
                            return;
                        }
                    }
                }
            }
        })
    }
}
