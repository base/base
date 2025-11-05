use crate::client::ClientConnection;
use crate::metrics::Metrics;
use axum::extract::ws::Message;
use futures::stream::StreamExt;
use futures::SinkExt;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::Sender;
use tokio::time::{interval, timeout, Duration};
use tracing::{debug, info, trace, warn};

fn get_message_size(msg: &Message) -> u64 {
    match msg {
        Message::Text(text) => text.len() as u64,
        Message::Binary(data) => data.len() as u64,
        Message::Ping(data) => data.len() as u64,
        Message::Pong(data) => data.len() as u64,
        Message::Close(_) => 0,
    }
}

#[derive(Clone)]
pub struct Registry {
    sender: Sender<Message>,
    metrics: Arc<Metrics>,
    compressed: bool,
    ping_enabled: bool,
    pong_timeout_ms: u64,
    send_timeout_ms: u64,
}

impl Registry {
    pub fn new(
        sender: Sender<Message>,
        metrics: Arc<Metrics>,
        compressed: bool,
        ping_enabled: bool,
        pong_timeout_ms: u64,
        send_timeout_ms: u64,
    ) -> Self {
        Self {
            sender,
            metrics,
            compressed,
            ping_enabled,
            pong_timeout_ms,
            send_timeout_ms,
        }
    }

    pub async fn subscribe(&self, client: ClientConnection) {
        info!(message = "subscribing client", client = client.id());

        let mut receiver = self.sender.subscribe();
        let metrics = self.metrics.clone();
        metrics.new_connections.increment(1);

        let filter = client.filter.clone();
        let compressed = self.compressed;
        let client_id = client.id();
        let (mut ws_sender, ws_receiver) = client.websocket.split();

        let (pong_error_tx, mut pong_error_rx) = tokio::sync::oneshot::channel();
        let client_reader = self.start_reader(ws_receiver, client_id.clone(), pong_error_tx);

        loop {
            tokio::select! {
                broadcast_result = receiver.recv() => {
                    match broadcast_result {
                        Ok(msg) => {
                            let msg_bytes = match &msg {
                                Message::Binary(data) => data.as_ref(),
                                _ => &[],
                            };
                            if filter.matches(msg_bytes, compressed) {
                                trace!(message = "filter matched for client", client = client_id, filter = ?filter);

                                let send_start = Instant::now();
                                let send_timeout = Duration::from_millis(self.send_timeout_ms);
                                let send_result = timeout(send_timeout, ws_sender.send(msg.clone())).await;
                                let send_duration = send_start.elapsed();
                                
                                metrics.message_send_duration.record(send_duration);

                                match send_result {
                                    Ok(Ok(())) => {
                                        // Success - message sent
                                        trace!(message = "message sent to client", client = client_id);
                                        metrics.sent_messages.increment(1);
                                        metrics.bytes_broadcasted.increment(get_message_size(&msg));
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
                                            timeout_ms = send_timeout.as_millis()
                                        );
                                        metrics.failed_messages.increment(1);
                                        break;
                                    }
                                }
                            } else {
                                trace!("Filter did not match for client {}", client_id);
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
        let metrics = self.metrics.clone();

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
