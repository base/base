//! Block subscription and first-seen timestamp tracking.
//!
//! Supports both WebSocket (`ws://` / `wss://`) via `newHeads` subscription
//! and HTTP (`http://` / `https://`) via polling `eth_blockNumber`.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_provider::{Provider, ProviderBuilder, WsConnect};
use futures::StreamExt;
use parking_lot::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace};
use url::Url;

/// Shared map of block numbers to their first-seen timestamps.
pub type BlockFirstSeen = Arc<RwLock<BTreeMap<u64, Instant>>>;

/// Maximum blocks retained (~17 minutes at 1 block/s).
const MAX_BLOCK_CACHE_SIZE: usize = 1000;

const HTTP_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Tracks when each block is first seen, via WebSocket subscription or HTTP polling.
#[derive(Debug)]
pub struct BlockWatcher {
    url: Url,
    block_first_seen: BlockFirstSeen,
    cancel_token: CancellationToken,
}

impl BlockWatcher {
    /// Creates a new [`BlockWatcher`].
    pub const fn new(
        url: Url,
        block_first_seen: BlockFirstSeen,
        cancel_token: CancellationToken,
    ) -> Self {
        Self { url, block_first_seen, cancel_token }
    }

    /// Spawns the watcher as a background task.
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    fn is_http(&self) -> bool {
        matches!(self.url.scheme(), "http" | "https")
    }

    async fn run(&self) {
        if self.is_http() {
            self.run_http_poll().await;
        } else {
            self.run_ws_subscribe().await;
        }
    }

    async fn run_ws_subscribe(&self) {
        info!(url = %self.url, "starting block watcher (websocket)");

        let mut backoff = Duration::from_millis(100);
        let max_backoff = Duration::from_secs(5);

        while !self.cancel_token.is_cancelled() {
            let ws = WsConnect::new(self.url.as_str());
            match ProviderBuilder::new().connect_ws(ws).await {
                Ok(provider) => {
                    info!("block watcher websocket connected");
                    backoff = Duration::from_millis(100);

                    match provider.subscribe_blocks().await {
                        Ok(sub) => {
                            let mut stream = sub.into_stream();
                            loop {
                                tokio::select! {
                                    biased;

                                    _ = self.cancel_token.cancelled() => {
                                        debug!("block watcher stopping");
                                        return;
                                    }
                                    header = stream.next() => {
                                        match header {
                                            Some(header) => {
                                                self.record_block(header.number);
                                            }
                                            None => {
                                                info!("block watcher subscription stream ended");
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error!(error = %e, "failed to subscribe to blocks");
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

    async fn run_http_poll(&self) {
        info!(url = %self.url, "starting block watcher (http polling)");

        let provider = ProviderBuilder::new().connect_http(self.url.clone());
        let mut last_block: Option<u64> = None;

        loop {
            tokio::select! {
                biased;

                _ = self.cancel_token.cancelled() => {
                    debug!("block watcher stopping");
                    return;
                }
                _ = tokio::time::sleep(HTTP_POLL_INTERVAL) => {}
            }

            match provider.get_block_number().await {
                Ok(block_number) => {
                    if last_block.is_none_or(|last| block_number > last) {
                        self.record_block(block_number);
                        last_block = Some(block_number);
                    }
                }
                Err(e) => {
                    debug!(error = %e, "failed to poll block number");
                }
            }
        }
    }

    fn record_block(&self, block_number: u64) {
        let now = Instant::now();
        trace!(block = block_number, "received new block");

        let mut blocks = self.block_first_seen.write();
        blocks.entry(block_number).or_insert(now);

        while blocks.len() > MAX_BLOCK_CACHE_SIZE {
            if blocks.pop_first().is_none() {
                break;
            }
        }
    }
}
