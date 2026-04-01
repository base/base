//! Flashblock subscription and transaction tracking.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_primitives::{TxHash, keccak256};
use base_alloy_flashblocks::Flashblock;
use futures::StreamExt;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
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
    stop_flag: Arc<AtomicBool>,
}

impl FlashblockTracker {
    /// Creates a new flashblock tracker.
    pub const fn new(
        ws_url: Url,
        flashblock_times: FlashblockTimes,
        stop_flag: Arc<AtomicBool>,
    ) -> Self {
        Self { ws_url, flashblock_times, stop_flag }
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

        while !self.stop_flag.load(Ordering::SeqCst) {
            match connect_async(self.ws_url.as_str()).await {
                Ok((ws_stream, _)) => {
                    info!("flashblock websocket connected");
                    backoff = Duration::from_millis(100);

                    let (_, mut read) = ws_stream.split();

                    loop {
                        if self.stop_flag.load(Ordering::SeqCst) {
                            debug!("flashblock tracker stopping");
                            return;
                        }

                        tokio::select! {
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(Message::Binary(data))) => {
                                        self.process_message(data.to_vec());
                                    }
                                    Some(Ok(Message::Text(data))) => {
                                        self.process_message(data.as_bytes().to_vec());
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
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                                if self.stop_flag.load(Ordering::SeqCst) {
                                    debug!("flashblock tracker stopping");
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
                    error!(error = %e, backoff_ms = backoff.as_millis(), "flashblock connection failed, retrying");
                }
            }

            if !self.stop_flag.load(Ordering::SeqCst) {
                tokio::time::sleep(backoff).await;
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

                let mut times = self.flashblock_times.write();
                for tx_bytes in &flashblock.diff.transactions {
                    if let Ok(tx_hash) = Self::extract_tx_hash(tx_bytes) {
                        times.entry(tx_hash).or_insert(now);
                    }
                }

                if times.len() > MAX_FLASHBLOCK_CACHE_SIZE {
                    let mut entries: Vec<_> = times.drain().collect();
                    entries.sort_by_key(|(_, t)| *t);
                    let keep_from = entries.len().saturating_sub(MAX_FLASHBLOCK_CACHE_SIZE);
                    times.extend(entries.into_iter().skip(keep_from));
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
