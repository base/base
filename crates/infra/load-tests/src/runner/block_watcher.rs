//! Block subscription and first-seen timestamp tracking via `newHeads` WebSocket.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::RwLock;
use serde::Deserialize;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};
use url::Url;

/// Shared map of block numbers to their first-seen timestamps.
pub type BlockFirstSeen = Arc<RwLock<HashMap<u64, Instant>>>;

#[derive(Debug, Deserialize)]
struct SubscribeConfirmation {
    result: Option<String>,
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct SubscriptionResponse {
    #[serde(default)]
    params: Option<SubscriptionParams>,
}

#[derive(Debug, Deserialize)]
struct SubscriptionParams {
    result: BlockHeader,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockHeader {
    number: String,
}

/// Maximum blocks retained (~17 minutes at 1 block/s).
const MAX_BLOCK_CACHE_SIZE: usize = 1000;

/// Subscribes to newHeads and tracks when each block is first seen.
#[derive(Debug)]
pub struct BlockWatcher {
    ws_url: Url,
    block_first_seen: BlockFirstSeen,
    cancel_token: CancellationToken,
}

impl BlockWatcher {
    /// Creates a new [`BlockWatcher`].
    pub const fn new(
        ws_url: Url,
        block_first_seen: BlockFirstSeen,
        cancel_token: CancellationToken,
    ) -> Self {
        Self { ws_url, block_first_seen, cancel_token }
    }

    /// Spawns the watcher as a background task.
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    async fn run(&self) {
        info!(url = %self.ws_url, "starting block watcher");

        let mut backoff = Duration::from_millis(100);
        let max_backoff = Duration::from_secs(5);

        while !self.cancel_token.is_cancelled() {
            match connect_async(self.ws_url.as_str()).await {
                Ok((ws_stream, _)) => {
                    info!("block watcher websocket connected");
                    backoff = Duration::from_millis(100);

                    let (mut write, mut read) = futures::StreamExt::split(ws_stream);

                    let subscribe_msg = r#"{"jsonrpc":"2.0","id":1,"method":"eth_subscribe","params":["newHeads"]}"#;
                    if let Err(e) =
                        futures::SinkExt::send(&mut write, Message::Text(subscribe_msg.into()))
                            .await
                    {
                        error!(error = %e, "failed to send subscription request");
                        continue;
                    }

                    if !self.await_subscription_confirmation(&mut read).await {
                        continue;
                    }

                    loop {
                        tokio::select! {
                            biased;

                            _ = self.cancel_token.cancelled() => {
                                debug!("block watcher stopping");
                                return;
                            }
                            msg = futures::StreamExt::next(&mut read) => {
                                match msg {
                                    Some(Ok(Message::Text(data))) => {
                                        self.process_message(&data);
                                    }
                                    Some(Ok(Message::Close(_))) => {
                                        info!("block watcher websocket closed by server");
                                        break;
                                    }
                                    Some(Ok(_)) => {}
                                    Some(Err(e)) => {
                                        warn!(error = %e, "block watcher websocket error");
                                        break;
                                    }
                                    None => {
                                        info!("block watcher websocket stream ended");
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    if self.cancel_token.is_cancelled() {
                        return;
                    }
                    error!(error = %e, backoff_ms = backoff.as_millis(), "block watcher connection failed, retrying");
                }
            }

            if !self.cancel_token.is_cancelled() {
                tokio::select! {
                    biased;
                    _ = self.cancel_token.cancelled() => return,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(max_backoff);
            }
        }

        debug!("block watcher stopped");
    }

    async fn await_subscription_confirmation(
        &self,
        read: &mut futures::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
    ) -> bool {
        let confirmation = tokio::select! {
            biased;
            _ = self.cancel_token.cancelled() => return false,
            msg = futures::StreamExt::next(read) => msg,
        };

        match confirmation {
            Some(Ok(Message::Text(data))) => {
                match serde_json::from_str::<SubscribeConfirmation>(&data) {
                    Ok(conf) if conf.error.is_some() => {
                        error!(error = ?conf.error, "eth_subscribe rejected");
                        false
                    }
                    Ok(conf) if conf.result.is_some() => {
                        debug!(subscription_id = ?conf.result, "subscription confirmed");
                        true
                    }
                    Ok(_) => {
                        warn!(data = %data, "unexpected subscription response");
                        false
                    }
                    Err(e) => {
                        warn!(error = %e, data = %data, "failed to parse subscription response");
                        false
                    }
                }
            }
            Some(Ok(Message::Close(_))) | None => {
                warn!("connection closed before subscription confirmed");
                false
            }
            Some(Ok(_)) => {
                warn!("unexpected message type for subscription confirmation");
                false
            }
            Some(Err(e)) => {
                warn!(error = %e, "websocket error awaiting subscription confirmation");
                false
            }
        }
    }

    fn process_message(&self, data: &str) {
        let now = Instant::now();

        let response: SubscriptionResponse = match serde_json::from_str(data) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "failed to parse block header");
                return;
            }
        };

        if let Some(params) = response.params {
            let block_number = match parse_hex_u64(&params.result.number) {
                Ok(n) => n,
                Err(e) => {
                    warn!(error = %e, "failed to parse block number");
                    return;
                }
            };

            trace!(block = block_number, "received new block");

            let mut blocks = self.block_first_seen.write();
            blocks.entry(block_number).or_insert(now);

            if blocks.len() > MAX_BLOCK_CACHE_SIZE {
                let cutoff = block_number.saturating_sub(MAX_BLOCK_CACHE_SIZE as u64);
                blocks.retain(|&num, _| num > cutoff);
            }
        }
    }
}

fn parse_hex_u64(s: &str) -> Result<u64, std::num::ParseIntError> {
    u64::from_str_radix(s.trim_start_matches("0x"), 16)
}
