//! Flashblocks WebSocket consumer and replay server.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::Bytes;
use axum::Router;
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::extract::ws::{Message, WebSocket};
use axum::response::IntoResponse;
use axum::routing::get;
use base_common_flashblocks::{Flashblock, FlashblockDecodeError};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio::time::interval;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::error::BenchmarkError;

const PING_INTERVAL: Duration = Duration::from_secs(1);
const RECONNECT_DELAY_MIN: Duration = Duration::from_millis(500);
const RECONNECT_DELAY_MAX: Duration = Duration::from_secs(5);
const BROADCAST_CAPACITY: usize = 256;

pub struct FlashblocksClient {
    pub port: u16,
    collected: Arc<tokio::sync::Mutex<Vec<Flashblock>>>,
}

impl FlashblocksClient {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            collected: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    pub fn url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }

    pub async fn drain(&self) -> Vec<Flashblock> {
        std::mem::take(&mut *self.collected.lock().await)
    }

    /// Connect and collect flashblocks until `cancel` is triggered.
    ///
    /// Reconnects automatically with exponential backoff on failure.
    pub async fn run(&self, cancel: CancellationToken) {
        let url = self.url();
        let collected = Arc::clone(&self.collected);
        let mut delay = RECONNECT_DELAY_MIN;

        loop {
            if cancel.is_cancelled() {
                return;
            }

            match connect_async(&url).await {
                Ok((ws_stream, _)) => {
                    delay = RECONNECT_DELAY_MIN;
                    info!(url = %url, "flashblocks WS connected");
                    let (mut write, mut read) = ws_stream.split();
                    let mut ping_tick = interval(PING_INTERVAL);

                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = ping_tick.tick() => {
                                if write.send(TungsteniteMessage::Ping(bytes::Bytes::new())).await.is_err() {
                                    break;
                                }
                            }
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(TungsteniteMessage::Text(text))) => {
                                        match Flashblock::try_decode_message(Bytes::from(text.as_bytes().to_vec())) {
                                            Ok(fb) => collected.lock().await.push(fb),
                                            Err(e) => warn!(error = %e, "failed to decode flashblock text"),
                                        }
                                    }
                                    Some(Ok(TungsteniteMessage::Binary(data))) => {
                                        match Flashblock::try_decode_message(Bytes::from(data.to_vec())) {
                                            Ok(fb) => collected.lock().await.push(fb),
                                            Err(e) => warn!(error = %e, "failed to decode flashblock binary"),
                                        }
                                    }
                                    Some(Ok(TungsteniteMessage::Pong(_))) => {}
                                    Some(Err(e)) => {
                                        warn!(error = %e, "flashblocks WS error");
                                        break;
                                    }
                                    None | Some(Ok(TungsteniteMessage::Close(_))) => break,
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, url = %url, "flashblocks WS connect failed");
                }
            }

            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(RECONNECT_DELAY_MAX);
        }
    }
}

#[derive(Clone)]
struct ReplayState {
    tx: broadcast::Sender<Vec<u8>>,
}

async fn ws_handler(
    State(state): State<ReplayState>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_client(socket, addr, state.tx.subscribe()))
}

async fn handle_client(
    socket: WebSocket,
    addr: SocketAddr,
    mut rx: broadcast::Receiver<Vec<u8>>,
) {
    let (mut sender, _receiver) = socket.split();
    info!(addr = %addr, "flashblocks replay client connected");

    loop {
        match rx.recv().await {
            Ok(data) => {
                if sender.send(Message::Binary(data.into())).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(addr = %addr, skipped = n, "replay client lagged");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }

    info!(addr = %addr, "flashblocks replay client disconnected");
}

pub struct FlashblockReplayServer {
    tx: broadcast::Sender<Vec<u8>>,
}

impl FlashblockReplayServer {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self { tx }
    }

    pub fn broadcast(&self, data: Vec<u8>) {
        let _ = self.tx.send(data);
    }

    pub fn broadcast_all(&self, flashblocks: &[Flashblock]) {
        for fb in flashblocks {
            match serde_json::to_vec(fb) {
                Ok(data) => self.broadcast(data),
                Err(e) => warn!(error = %e, "failed to serialize flashblock for replay"),
            }
        }
    }

    pub async fn run(&self, port: u16, cancel: CancellationToken) -> Result<(), BenchmarkError> {
        let state = ReplayState { tx: self.tx.clone() };

        let router = Router::new()
            .route("/", get(ws_handler))
            .with_state(state);

        let listener = TcpListener::bind(("0.0.0.0", port))
            .await
            .map_err(BenchmarkError::Io)?;

        info!(port = port, "flashblocks replay server listening");

        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
        .map_err(BenchmarkError::Io)
    }
}

impl Default for FlashblockReplayServer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn flashblocks_client_drain_empty() {
        let client = FlashblocksClient::new(9990);
        assert!(client.drain().await.is_empty());
    }

    #[test]
    fn replay_server_broadcast_to_no_receivers_does_not_panic() {
        let server = FlashblockReplayServer::new();
        server.broadcast(b"hello".to_vec());
    }
}
