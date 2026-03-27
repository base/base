//! Block subscription and first-seen timestamp tracking.
//!
//! Subscribes to `newHeads` via WebSocket and records the timestamp when each
//! block is first observed. This enables accurate latency measurement independent
//! of polling delays.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::RwLock;
use serde::Deserialize;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{debug, error, info, trace, warn};
use url::Url;

/// Shared map of block numbers to their first-seen timestamps.
pub type BlockFirstSeen = Arc<RwLock<HashMap<u64, Instant>>>;

/// JSON-RPC subscription message for newHeads.
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

/// Maximum number of blocks to retain in memory.
/// At 1 block/second, this covers ~17 minutes of history.
const MAX_BLOCK_CACHE_SIZE: usize = 1000;

/// Subscribes to newHeads and tracks when each block is first seen.
#[derive(Debug)]
pub struct BlockWatcher {
    ws_url: Url,
    block_first_seen: BlockFirstSeen,
    stop_flag: Arc<AtomicBool>,
}

impl BlockWatcher {
    /// Creates a new block watcher.
    pub const fn new(
        ws_url: Url,
        block_first_seen: BlockFirstSeen,
        stop_flag: Arc<AtomicBool>,
    ) -> Self {
        Self { ws_url, block_first_seen, stop_flag }
    }

    /// Starts the block watcher in a background task.
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    async fn run(&self) {
        info!(url = %self.ws_url, "starting block watcher");

        let mut backoff = Duration::from_millis(100);
        let max_backoff = Duration::from_secs(5);

        while !self.stop_flag.load(Ordering::SeqCst) {
            match connect_async(self.ws_url.as_str()).await {
                Ok((ws_stream, _)) => {
                    info!("block watcher websocket connected");
                    backoff = Duration::from_millis(100);

                    let (mut write, mut read) = futures::StreamExt::split(ws_stream);

                    // Subscribe to newHeads
                    let subscribe_msg = r#"{"jsonrpc":"2.0","id":1,"method":"eth_subscribe","params":["newHeads"]}"#;
                    if let Err(e) = futures::SinkExt::send(
                        &mut write,
                        Message::Text(subscribe_msg.to_string().into()),
                    )
                    .await
                    {
                        error!(error = %e, "failed to send subscription request");
                        continue;
                    }

                    loop {
                        if self.stop_flag.load(Ordering::SeqCst) {
                            debug!("block watcher stopping");
                            return;
                        }

                        tokio::select! {
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
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                                if self.stop_flag.load(Ordering::SeqCst) {
                                    debug!("block watcher stopping");
                                    return;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    if self.stop_flag.load(Ordering::SeqCst) {
                        return;
                    }
                    error!(error = %e, backoff_ms = backoff.as_millis(), "block watcher connection failed, retrying");
                }
            }

            if !self.stop_flag.load(Ordering::SeqCst) {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        }

        debug!("block watcher stopped");
    }

    fn process_message(&self, data: &str) {
        let now = Instant::now();

        let response: SubscriptionResponse = match serde_json::from_str(data) {
            Ok(r) => r,
            Err(e) => {
                // Subscription confirmation messages don't have params
                if data.contains("\"result\":\"0x") {
                    debug!("received subscription confirmation");
                    return;
                }
                warn!(error = %e, data = %data, "failed to parse block header");
                return;
            }
        };

        // Handle subscription notification
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

            // Prune old blocks to prevent unbounded memory growth
            if blocks.len() > MAX_BLOCK_CACHE_SIZE {
                let cutoff = block_number.saturating_sub(MAX_BLOCK_CACHE_SIZE as u64);
                blocks.retain(|&num, _| num > cutoff);
            }
        }
    }
}

/// Parses a hex string (with 0x prefix) to u64.
fn parse_hex_u64(s: &str) -> Result<u64, std::num::ParseIntError> {
    u64::from_str_radix(s.trim_start_matches("0x"), 16)
}
