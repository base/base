//! Builder flashblocks broadcast transaction watching.

use std::time::{Duration, Instant};

use alloy_eips::Decodable2718;
use alloy_primitives::{TxHash, keccak256};
use base_common_consensus::{Eip8130Constants, Eip8130Signed};
use base_common_flashblocks::Flashblock;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Bytes, protocol::Message},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use url::Url;

use super::{FlashblockInclusion, InclusionPulse, ResultsTracker};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Watches transaction inclusion times from the builder flashblocks broadcast WebSocket.
#[derive(Debug)]
pub struct FlashblockWatcher {
    ws_url: Url,
    results_tracker: ResultsTracker,
    pulse_tx: mpsc::Sender<InclusionPulse>,
    cancel_token: CancellationToken,
}

impl FlashblockWatcher {
    /// Creates a new [`FlashblockWatcher`].
    pub const fn new(
        ws_url: Url,
        results_tracker: ResultsTracker,
        pulse_tx: mpsc::Sender<InclusionPulse>,
        cancel_token: CancellationToken,
    ) -> Self {
        Self { ws_url, results_tracker, pulse_tx, cancel_token }
    }

    /// Spawns the watcher as a background task.
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    async fn run(&self) {
        info!("starting flashblock watcher");

        let mut backoff = Duration::from_millis(100);
        let max_backoff = Duration::from_secs(5);
        let mut failed_attempts = 0u64;
        let mut has_connected = false;
        let mut outage_reported = false;
        let mut last_failure_warning =
            Instant::now().checked_sub(Duration::from_secs(15)).unwrap_or_else(Instant::now);

        while !self.cancel_token.is_cancelled() {
            match tokio::time::timeout(CONNECT_TIMEOUT, connect_async(self.ws_url.as_str())).await {
                Ok(Ok((ws_stream, _))) => {
                    if outage_reported {
                        info!(failed_attempts, "flashblock websocket recovered");
                        outage_reported = false;
                    } else if !has_connected {
                        info!("flashblock websocket connected");
                    }
                    has_connected = true;
                    failed_attempts = 0;
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
                                        self.process_message(data).await;
                                    }
                                    Some(Ok(Message::Text(data))) => {
                                        self.process_message(Bytes::from(data)).await;
                                    }
                                    Some(Ok(Message::Close(_))) => {
                                        debug!("flashblock websocket closed by server");
                                        break;
                                    }
                                    Some(Ok(_)) => {}
                                    Some(Err(e)) => {
                                        failed_attempts = failed_attempts.saturating_add(1);
                                        if last_failure_warning.elapsed() >= Duration::from_secs(15) {
                                            warn!(
                                                error = %e,
                                                failed_attempts,
                                                "flashblock websocket stream failing"
                                            );
                                            outage_reported = true;
                                            last_failure_warning = Instant::now();
                                        }
                                        break;
                                    }
                                    None => {
                                        debug!("flashblock websocket stream ended");
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    if self.cancel_token.is_cancelled() {
                        return;
                    }
                    failed_attempts = failed_attempts.saturating_add(1);
                    if last_failure_warning.elapsed() >= Duration::from_secs(15) {
                        warn!(
                            error = %e,
                            failed_attempts,
                            backoff_ms = backoff.as_millis(),
                            "flashblock connection failing, retrying"
                        );
                        outage_reported = true;
                        last_failure_warning = Instant::now();
                    }
                }
                Err(_) => {
                    failed_attempts = failed_attempts.saturating_add(1);
                    if last_failure_warning.elapsed() >= Duration::from_secs(15) {
                        warn!(
                            timeout_secs = CONNECT_TIMEOUT.as_secs(),
                            failed_attempts,
                            backoff_ms = backoff.as_millis(),
                            "flashblock connection timing out, retrying"
                        );
                        outage_reported = true;
                        last_failure_warning = Instant::now();
                    }
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

    async fn process_message(&self, bytes: Bytes) {
        let now = Instant::now();

        let flashblock = match Flashblock::try_decode_message(bytes) {
            Ok(flashblock) => flashblock,
            Err(e) => {
                warn!(error = %e, "failed to decode flashblock broadcast message");
                return;
            }
        };

        let inclusions = flashblock
            .diff
            .transactions
            .iter()
            .map(|tx_bytes| FlashblockInclusion {
                tx_hash: inclusion_hash(tx_bytes),
                included_at: now,
            })
            .collect();
        self.publish_inclusions(inclusions, now).await;
    }

    async fn publish_inclusions(&self, inclusions: Vec<FlashblockInclusion>, observed_at: Instant) {
        let block_match = self.results_tracker.on_new_flashblock(inclusions);
        if block_match.released_gas > 0 {
            let _ = self
                .pulse_tx
                .send(InclusionPulse::flashblock(observed_at, block_match.released_gas))
                .await;
        }
    }
}

/// Returns the transaction hash of a flashblock transaction's EIP-2718 bytes.
///
/// For every type except EIP-8130 this is `keccak256` of the bytes. An EIP-8130
/// hash excludes `sender_auth` and `payer_auth`, so it is decoded and taken
/// from [`Eip8130Signed::hash`] to match the hash recorded at submission.
fn inclusion_hash(tx_bytes: &[u8]) -> TxHash {
    if tx_bytes.first() == Some(&Eip8130Constants::EIP8130_TX_TYPE)
        && let Ok(signed) = Eip8130Signed::decode_2718_exact(tx_bytes)
    {
        return *signed.hash();
    }
    keccak256(tx_bytes)
}

#[cfg(test)]
mod tests {
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, Bytes as TxBytes};
    use base_common_consensus::TxEip8130;
    use tokio::sync::mpsc;

    use super::*;
    use crate::runner::{InclusionSource, SentTransaction, SubmitCohort};

    #[test]
    fn inclusion_hash_uses_eip8130_transaction_hash() {
        let tx = TxEip8130 {
            chain_id: 8453,
            sender: Some(Address::with_last_byte(1)),
            ..Default::default()
        };
        let signed = Eip8130Signed::new(tx, TxBytes::from_static(&[0xAB; 32]), TxBytes::new());
        let raw = signed.encoded_2718();

        assert_ne!(*signed.hash(), keccak256(&raw));
        assert_eq!(inclusion_hash(&raw), *signed.hash());
        assert_eq!(inclusion_hash(&[0x02, 0xc0]), keccak256([0x02, 0xc0]));
    }

    #[tokio::test]
    async fn matching_flashblock_publishes_early_refill_pulse() {
        let sender = Address::with_last_byte(1);
        let tx_hash = TxHash::repeat_byte(2);
        let tracker = ResultsTracker::new(&[sender]);
        tracker.begin_measurement(0, None);
        tracker.sent_transactions(vec![SentTransaction {
            tx_hash,
            from: sender,
            estimated_gas: 21_000,
            measured: true,
            cohort: SubmitCohort::Plain,
        }]);
        let (pulse_tx, mut pulse_rx) = mpsc::channel(1);
        let watcher = FlashblockWatcher::new(
            "ws://localhost:7111".parse().expect("valid test URL"),
            tracker,
            pulse_tx,
            CancellationToken::new(),
        );
        let observed_at = Instant::now();

        watcher
            .publish_inclusions(
                vec![FlashblockInclusion { tx_hash, included_at: observed_at }],
                observed_at,
            )
            .await;

        let pulse = pulse_rx.recv().await.expect("matching inclusion emits a pulse");
        assert_eq!(pulse.source, InclusionSource::Flashblock);
        assert_eq!(pulse.released_gas, 21_000);
        assert!(pulse.canonical.is_none());
    }
}
