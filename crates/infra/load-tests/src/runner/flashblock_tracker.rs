//! Flashblock subscription and transaction tracking.

use std::{
    collections::{VecDeque, hash_map::Entry},
    time::{Duration, Instant},
};

use alloy_primitives::{TxHash, keccak256};
use base_common_flashblocks::Flashblock;
use futures::StreamExt;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Bytes, protocol::Message},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};
use url::Url;

use super::FlashblockTimes;

/// Cap on the flashblock times map; receives all txs, not just ours.
const MAX_FLASHBLOCK_CACHE_SIZE: usize = 50_000;

/// Subscribes to flashblocks and tracks transaction inclusion times.
#[derive(Debug)]
pub struct FlashblockTracker {
    ws_url: Url,
    flashblock_times: FlashblockTimes,
    cancel_token: CancellationToken,
    /// Tracks insertion order for FIFO eviction. Persists across reconnects
    /// so entries from prior connections remain evictable.
    eviction_queue: VecDeque<TxHash>,
}

impl FlashblockTracker {
    /// Creates a new [`FlashblockTracker`].
    pub const fn new(
        ws_url: Url,
        flashblock_times: FlashblockTimes,
        cancel_token: CancellationToken,
    ) -> Self {
        Self { ws_url, flashblock_times, cancel_token, eviction_queue: VecDeque::new() }
    }

    /// Spawns the tracker as a background task.
    pub fn start(mut self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    async fn run(&mut self) {
        info!(url = %self.ws_url, "starting flashblock tracker");

        let mut backoff = Duration::from_millis(100);
        let max_backoff = Duration::from_secs(5);

        while !self.cancel_token.is_cancelled() {
            match connect_async(self.ws_url.as_str()).await {
                Ok((ws_stream, _)) => {
                    info!("flashblock websocket connected");
                    backoff = Duration::from_millis(100);

                    let (_, mut read) = ws_stream.split();

                    loop {
                        tokio::select! {
                            biased;

                            _ = self.cancel_token.cancelled() => {
                                debug!("flashblock tracker stopping");
                                return;
                            }
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(Message::Binary(data))) => {
                                        self.process_message(data);
                                    }
                                    Some(Ok(Message::Text(data))) => {
                                        self.process_message(Bytes::from(data));
                                    }
                                    Some(Ok(Message::Close(_))) => {
                                        info!("flashblock websocket closed by server");
                                        break;
                                    }
                                    Some(Ok(_)) => {}
                                    Some(Err(e)) => {
                                        warn!(error = %e, "flashblock websocket error");
                                        break;
                                    }
                                    None => {
                                        info!("flashblock websocket stream ended");
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
                    error!(error = %e, backoff_ms = backoff.as_millis(), "flashblock connection failed, retrying");
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

        debug!("flashblock tracker stopped");
    }

    fn process_message(&mut self, bytes: Bytes) {
        let now = Instant::now();

        match Flashblock::try_decode_message(bytes) {
            Ok(flashblock) => {
                let tx_count = flashblock.diff.transactions.len();
                trace!(index = flashblock.index, tx_count, "received flashblock");

                let tx_hashes: Vec<TxHash> = flashblock
                    .diff
                    .transactions
                    .iter()
                    .filter_map(|tx_bytes| Self::extract_tx_hash(tx_bytes).ok())
                    .collect();

                let mut times = self.flashblock_times.write();
                for tx_hash in tx_hashes {
                    if let Entry::Vacant(e) = times.entry(tx_hash) {
                        e.insert(now);
                        self.eviction_queue.push_back(tx_hash);
                    }
                }

                while times.len() > MAX_FLASHBLOCK_CACHE_SIZE {
                    match self.eviction_queue.pop_front() {
                        Some(old) => {
                            times.remove(&old);
                        }
                        None => break,
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to decode flashblock");
            }
        }
    }

    fn extract_tx_hash(tx_bytes: &[u8]) -> Result<TxHash, &'static str> {
        if tx_bytes.is_empty() {
            return Err("empty transaction bytes");
        }
        Ok(keccak256(tx_bytes))
    }
}
