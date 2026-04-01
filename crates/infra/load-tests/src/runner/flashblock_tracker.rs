//! Flashblock subscription and transaction tracking.

use std::time::{Duration, Instant};

use alloy_primitives::{TxHash, keccak256};
use base_alloy_flashblocks::Flashblock;
use futures::StreamExt;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Bytes, protocol::Message},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};
use url::Url;

use super::FlashblockTimes;

/// Maximum number of transaction entries to retain in the flashblock times map.
/// When exceeded, the oldest entries by timestamp are dropped to enforce this cap,
/// since the map receives every transaction in every flashblock (not just ours).
const MAX_FLASHBLOCK_CACHE_SIZE: usize = 50_000;

/// Subscribes to flashblocks and tracks transaction inclusion times.
#[derive(Debug)]
pub struct FlashblockTracker {
    ws_url: Url,
    flashblock_times: FlashblockTimes,
    cancel_token: CancellationToken,
}

impl FlashblockTracker {
    /// Creates a new flashblock tracker.
    pub const fn new(
        ws_url: Url,
        flashblock_times: FlashblockTimes,
        cancel_token: CancellationToken,
    ) -> Self {
        Self { ws_url, flashblock_times, cancel_token }
    }

    /// Starts the flashblock subscription in a background task.
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    async fn run(&self) {
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
                                        self.process_message(data.into());
                                    }
                                    Some(Ok(Message::Text(data))) => {
                                        self.process_message(Bytes::from(data).into());
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

    fn process_message(&self, bytes: Vec<u8>) {
        let now = Instant::now();

        match Flashblock::try_decode_message(bytes) {
            Ok(flashblock) => {
                let tx_count = flashblock.diff.transactions.len();
                trace!(index = flashblock.index, tx_count, "received flashblock");

                // Compute tx hashes outside the lock (keccak256 is pure)
                let tx_hashes: Vec<TxHash> = flashblock
                    .diff
                    .transactions
                    .iter()
                    .filter_map(|tx_bytes| Self::extract_tx_hash(tx_bytes).ok())
                    .collect();

                // Acquire lock only for insertion + pruning
                let mut times = self.flashblock_times.write();
                for tx_hash in tx_hashes {
                    times.entry(tx_hash).or_insert(now);
                }

                // Prune using retain with a cutoff instead of drain/sort/re-insert
                if times.len() > MAX_FLASHBLOCK_CACHE_SIZE {
                    // Find the cutoff time: keep entries newer than this
                    let mut timestamps: Vec<Instant> = times.values().copied().collect();
                    timestamps.sort_unstable();
                    let keep_count = MAX_FLASHBLOCK_CACHE_SIZE;
                    let cutoff_idx = timestamps.len().saturating_sub(keep_count);
                    let cutoff_time = timestamps[cutoff_idx];

                    times.retain(|_, &mut t| t > cutoff_time);
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
