//! Builder flashblocks broadcast transaction watching.

use std::time::{Duration, Instant};

use alloy_eips::eip2718::Decodable2718;
use base_common_consensus::BaseTxEnvelope;
use base_common_flashblocks::Flashblock;
use futures::StreamExt;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Bytes, protocol::Message},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use url::Url;

use super::{FlashblockInclusion, ResultsTracker};

/// Watches transaction inclusion times from the builder flashblocks broadcast WebSocket.
#[derive(Debug)]
pub struct FlashblockWatcher {
    ws_url: Url,
    results_tracker: ResultsTracker,
    cancel_token: CancellationToken,
}

impl FlashblockWatcher {
    /// Creates a new [`FlashblockWatcher`].
    pub const fn new(
        ws_url: Url,
        results_tracker: ResultsTracker,
        cancel_token: CancellationToken,
    ) -> Self {
        Self { ws_url, results_tracker, cancel_token }
    }

    /// Spawns the watcher as a background task.
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    async fn run(&self) {
        info!(url = %self.ws_url, "starting flashblock watcher");

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
                                debug!("flashblock watcher stopping");
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

        debug!("flashblock watcher stopped");
    }

    fn process_message(&self, bytes: Bytes) {
        let now = Instant::now();

        let flashblock = match Flashblock::try_decode_message(bytes) {
            Ok(flashblock) => flashblock,
            Err(e) => {
                warn!(error = %e, "failed to decode flashblock broadcast message");
                return;
            }
        };

        let inclusions = Self::parse_broadcast_inclusions(&flashblock, now);
        self.results_tracker.on_new_flashblock(inclusions);
    }

    fn parse_broadcast_inclusions(
        flashblock: &Flashblock,
        included_at: Instant,
    ) -> Vec<FlashblockInclusion> {
        flashblock
            .diff
            .transactions
            .iter()
            .filter_map(|tx_bytes| {
                let envelope = BaseTxEnvelope::decode_2718_exact(tx_bytes.as_ref())
                    .inspect_err(|e| warn!(error = %e, "failed to decode flashblock transaction"))
                    .ok()?;
                Some(FlashblockInclusion { tx_hash: envelope.tx_hash(), included_at })
            })
            .collect()
    }
}
