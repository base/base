//! Lightweight canonical head observation over `eth_subscribe(newHeads)`.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_provider::{Provider, ProviderBuilder};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

use super::{BlockPulse, InclusionPulse, ResultsTracker};

/// Observes canonical headers without downloading each block's transaction hash list.
#[derive(Debug)]
pub(crate) struct CanonicalHeadWatcher {
    ws_url: Url,
    results_tracker: ResultsTracker,
    block_time: Duration,
    pulse_tx: mpsc::Sender<InclusionPulse>,
    active: Arc<AtomicBool>,
    cancel_token: CancellationToken,
}

impl CanonicalHeadWatcher {
    /// Creates a canonical head watcher.
    pub(crate) const fn new(
        ws_url: Url,
        results_tracker: ResultsTracker,
        block_time: Duration,
        pulse_tx: mpsc::Sender<InclusionPulse>,
        active: Arc<AtomicBool>,
        cancel_token: CancellationToken,
    ) -> Self {
        Self { ws_url, results_tracker, block_time, pulse_tx, active, cancel_token }
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
        self.active.store(true, Ordering::Release);

        let mut stream = subscription.into_stream();
        let mut last_number = None;
        let mut expected_boundary = None;
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

            let observed_at = Instant::now();
            let blocks_advanced = last_number.map_or(1, |last| number.saturating_sub(last).max(1));
            let boundary = expected_boundary.map_or(observed_at, |expected: Instant| {
                expected
                    + self.block_time.saturating_mul(
                        u32::try_from(blocks_advanced.saturating_sub(1)).unwrap_or(u32::MAX),
                    )
            });
            expected_boundary = Some(boundary + self.block_time);
            last_number = Some(number);

            self.results_tracker.observe_measurement_block(number);
            let pulse = BlockPulse {
                number,
                gas_used: header.gas_used,
                gas_limit: header.gas_limit,
                base_fee: u128::from(header.base_fee_per_gas.unwrap_or_default()),
                our_included_gas: 0,
                expected_boundary: boundary,
                observed_at,
            };
            if self.pulse_tx.send(InclusionPulse::canonical(pulse, 0)).await.is_err() {
                break;
            }
        }
        self.active.store(false, Ordering::Release);
    }
}
