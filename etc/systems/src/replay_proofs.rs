//! Proofs mode of `base-bench replay`: while blocks are inserted into a proofs-enabled node,
//! records how far proof history trails the head.

use std::{thread::JoinHandle, time::Duration};

use alloy_provider::{Provider, RootProvider};
use base_common_network::Base;
use eyre::{Result, WrapErr};
use serde::Serialize;
use tokio::sync::watch;
use tracing::warn;
use url::Url;

/// Bound on each probe call, so a node that stops answering cannot hold back the result.
const PROBE_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Nearest-rank percentiles of integer samples.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CountSummary {
    /// Number of samples.
    pub samples: u64,
    /// Nearest-rank median.
    pub p50: u64,
    /// Nearest-rank 99th percentile.
    pub p99: u64,
    /// Maximum.
    pub max: u64,
}

impl CountSummary {
    /// Summarizes samples; all fields are zero without samples.
    pub fn from_values(values: &[u64]) -> Self {
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let rank = |percentile: usize| {
            let index = (percentile * sorted.len()).div_ceil(100).max(1) - 1;
            sorted.get(index).copied().unwrap_or_default()
        };
        Self {
            samples: sorted.len() as u64,
            p50: rank(50),
            p99: rank(99),
            max: sorted.last().copied().unwrap_or_default(),
        }
    }
}

/// The `proofs` section of `replay-result.json`. Recorded only; it never fails the run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofsSummary {
    /// Inserted head minus `debug_proofsSyncStatus.latest`, sampled when the head moves; heads
    /// inserted during a status call are not sampled.
    pub head_lag_blocks: CountSummary,
    /// Status calls that failed, timed out or returned no `latest` block, and so gave no sample.
    pub failed_probes: u64,
}

/// Prober on its own thread and runtime, so its calls stay off the runtime that serves the
/// engine API.
#[derive(Debug)]
pub struct ReplayProofsProbe {
    /// Latest inserted block number, published by the replay loop.
    pub head: watch::Sender<u64>,
    thread: JoinHandle<Result<(Vec<u64>, u64)>>,
}

impl ReplayProofsProbe {
    /// Starts probing the node at `rpc_url` each time `head` moves.
    pub fn start(rpc_url: Url, boundary: u64) -> Self {
        let (head, head_rx) = watch::channel(boundary);
        let provider = RootProvider::<Base>::new_http(rpc_url);
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .wrap_err("failed to build the proofs probe runtime")?
                .block_on(Self::run(provider, head_rx))
        });
        Self { head, thread }
    }

    /// Stops probing once the current call finishes and summarizes the samples.
    pub async fn finish(self) -> Result<ProofsSummary> {
        drop(self.head);
        let thread = self.thread;
        let (lags, failed_probes) = tokio::task::spawn_blocking(move || thread.join())
            .await?
            .map_err(|_| eyre::eyre!("proofs probe thread panicked"))??;
        Ok(ProofsSummary { head_lag_blocks: CountSummary::from_values(&lags), failed_probes })
    }

    async fn run(
        provider: RootProvider<Base>,
        mut head_rx: watch::Receiver<u64>,
    ) -> Result<(Vec<u64>, u64)> {
        let mut lags = Vec::new();
        let mut failed = 0;
        while head_rx.changed().await.is_ok() {
            let head = *head_rx.borrow_and_update();
            let status =
                provider.raw_request::<_, serde_json::Value>("debug_proofsSyncStatus".into(), ());
            match tokio::time::timeout(PROBE_CALL_TIMEOUT, status).await {
                Ok(Ok(status)) => match status["latest"].as_u64() {
                    Some(latest) => lags.push(head.saturating_sub(latest)),
                    None => {
                        failed += 1;
                        warn!(%status, "debug_proofsSyncStatus returned no latest block");
                    }
                },
                Ok(Err(error)) => {
                    failed += 1;
                    warn!(%error, "debug_proofsSyncStatus failed");
                }
                Err(_) => {
                    failed += 1;
                    warn!("debug_proofsSyncStatus timed out");
                }
            }
        }
        Ok((lags, failed))
    }
}

#[cfg(test)]
mod tests {
    use super::CountSummary;

    #[test]
    fn count_summary_uses_nearest_rank_percentiles() {
        let values = (1..=200).rev().collect::<Vec<u64>>();

        assert_eq!(
            CountSummary::from_values(&values),
            CountSummary { samples: 200, p50: 100, p99: 198, max: 200 }
        );
        assert_eq!(
            CountSummary::from_values(&[0, 3, 1]),
            CountSummary { samples: 3, p50: 1, p99: 3, max: 3 }
        );
        assert_eq!(CountSummary::from_values(&[]), CountSummary::default());
    }
}
