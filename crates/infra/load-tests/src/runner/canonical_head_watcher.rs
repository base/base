//! Lightweight canonical head observation over `eth_subscribe(newHeads)`.

use alloy_provider::{Provider, ProviderBuilder};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

use super::ResultsTracker;

/// Observes canonical headers independently of transaction-hash scanning.
///
/// Header notifications establish the measurement boundary promptly. The polling block watcher
/// continues to scan full blocks and remains the sole source of inclusion-aware pacing pulses.
#[derive(Debug)]
pub(crate) struct CanonicalHeadWatcher {
    ws_url: Url,
    results_tracker: ResultsTracker,
    cancel_token: CancellationToken,
}

impl CanonicalHeadWatcher {
    /// Creates a canonical head watcher.
    pub(crate) const fn new(
        ws_url: Url,
        results_tracker: ResultsTracker,
        cancel_token: CancellationToken,
    ) -> Self {
        Self { ws_url, results_tracker, cancel_token }
    }

    /// Starts the watcher in a background task.
    pub(crate) fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }

    async fn run(&self) {
        let provider = match ProviderBuilder::new().connect(self.ws_url.as_str()).await {
            Ok(provider) => provider,
            Err(error) => {
                warn!(error = %error, ws_url = %self.ws_url, "canonical head WebSocket connection failed");
                return;
            }
        };
        let subscription = match provider.subscribe_blocks().await {
            Ok(subscription) => subscription,
            Err(error) => {
                warn!(error = %error, ws_url = %self.ws_url, "canonical head subscription failed");
                return;
            }
        };
        info!(ws_url = %self.ws_url, "started canonical head watcher");

        let mut stream = subscription.into_stream();
        let mut last_number = None;
        loop {
            let header = tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => break,
                header = stream.next() => {
                    let Some(header) = header else {
                        warn!("canonical head subscription ended");
                        break;
                    };
                    header
                }
            };
            let number = header.number;
            if last_number.is_some_and(|last| number <= last) {
                continue;
            }
            last_number = Some(number);
            self.results_tracker.observe_measurement_block(number);
        }
    }
}
