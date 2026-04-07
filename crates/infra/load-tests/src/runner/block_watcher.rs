//! Block subscription and first-seen timestamp tracking via `newHeads` WebSocket.

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
            let ws = WsConnect::new(self.ws_url.as_str());
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
                                                let now = Instant::now();
                                                let block_number = header.number;
                                                trace!(block = block_number, "received new block");

                                                let mut blocks = self.block_first_seen.write();
                                                blocks.entry(block_number).or_insert(now);

                                                while blocks.len() > MAX_BLOCK_CACHE_SIZE {
                                                    if blocks.pop_first().is_none() {
                                                        break;
                                                    }
                                                }
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
}
