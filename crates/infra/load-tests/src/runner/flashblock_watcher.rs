//! Builder flashblocks broadcast transaction watching.

use std::time::{Duration, Instant};

use alloy_primitives::keccak256;
use base_common_flashblocks::Flashblock;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Bytes, protocol::Message},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};
use url::Url;

use super::{FlashblockInclusion, ResultsTracker};

/// Bound on the decode queue. Captures receive timestamps under bursty load without
/// unbounded memory growth; a full channel signals the decoder cannot keep up.
const DECODE_QUEUE_CAPACITY: usize = 1024;

/// A raw flashblock message paired with the instant its bytes arrived on the socket.
struct ReceivedMessage {
    received_at: Instant,
    bytes: Bytes,
}

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
                    let (tx, rx) = mpsc::channel::<ReceivedMessage>(DECODE_QUEUE_CAPACITY);
                    let decoder = tokio::spawn(Self::decode_loop(rx, self.results_tracker.clone()));

                    loop {
                        tokio::select! {
                            biased;

                            _ = self.cancel_token.cancelled() => {
                                debug!("flashblock watcher stopping");
                                drop(tx);
                                let _ = decoder.await;
                                return;
                            }
                            msg = read.next() => {
                                let received_at = Instant::now();
                                match msg {
                                    Some(Ok(Message::Binary(data))) => {
                                        Self::enqueue(&tx, received_at, data);
                                    }
                                    Some(Ok(Message::Text(data))) => {
                                        Self::enqueue(&tx, received_at, Bytes::from(data));
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

                    drop(tx);
                    let _ = decoder.await;
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

    fn enqueue(tx: &mpsc::Sender<ReceivedMessage>, received_at: Instant, bytes: Bytes) {
        // A full queue means the decoder is lagging; dropping here keeps the read loop's
        // timestamps honest rather than letting socket backpressure inflate received_at.
        if let Err(e) = tx.try_send(ReceivedMessage { received_at, bytes }) {
            warn!(error = %e, "flashblock decode queue full, dropping message");
        }
    }

    async fn decode_loop(mut rx: mpsc::Receiver<ReceivedMessage>, results_tracker: ResultsTracker) {
        while let Some(ReceivedMessage { received_at, bytes }) = rx.recv().await {
            let flashblock = match Flashblock::try_decode_message(bytes) {
                Ok(flashblock) => flashblock,
                Err(e) => {
                    warn!(error = %e, "failed to decode flashblock broadcast message");
                    continue;
                }
            };

            let inclusions = Self::parse_broadcast_inclusions(&flashblock, received_at);
            trace!(
                processing_lag_ms = received_at.elapsed().as_millis(),
                tx_count = inclusions.len(),
                "decoded flashblock"
            );
            results_tracker.on_new_flashblock(inclusions);
        }
    }

    fn parse_broadcast_inclusions(
        flashblock: &Flashblock,
        included_at: Instant,
    ) -> Vec<FlashblockInclusion> {
        flashblock
            .diff
            .transactions
            .iter()
            .map(|tx_bytes| FlashblockInclusion { tx_hash: keccak256(tx_bytes), included_at })
            .collect()
    }
}
