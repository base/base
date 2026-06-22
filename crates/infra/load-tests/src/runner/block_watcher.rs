//! Block watching and transaction landing detection via `eth_getBlockByNumber`.
//!
//! The watcher polls for new canonical blocks and reports the transaction hashes
//! contained in each block to the [`ResultsTracker`], which records landing latency.
//! Canonical receipts (gas, revert status) are fetched separately in a single batch
//! pass at the end of the run via [`fetch_block_receipts`], not during polling.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy_network::ReceiptResponse;
use alloy_primitives::TxHash;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::{BlockId, BlockNumberOrTag};
use base_common_network::Base;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

use super::{BlockObservation, BlockReceipt, ResultsTracker};

/// How frequently to poll for a new canonical block.
const BLOCK_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Maximum time to wait for a block watcher RPC request.
const BLOCK_RPC_TIMEOUT: Duration = Duration::from_secs(10);
/// Small startup lookback so early landings are not missed if the watcher task is
/// scheduled after the first submissions.
const INITIAL_BLOCK_LOOKBACK: u64 = 8;

/// Polls canonical blocks and reports their transaction hashes for landing detection.
#[derive(Debug)]
pub struct BlockWatcher {
    provider: RootProvider<Base>,
    results_tracker: ResultsTracker,
    cancel_token: CancellationToken,
}

impl BlockWatcher {
    /// Creates a new [`BlockWatcher`].
    pub const fn new(
        provider: RootProvider<Base>,
        results_tracker: ResultsTracker,
        cancel_token: CancellationToken,
    ) -> Self {
        Self { provider, results_tracker, cancel_token }
    }

    /// Spawns the watcher as a background task.
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    async fn run(&self) {
        info!("started block watcher");

        let mut backoff = Duration::from_millis(100);
        let max_backoff = Duration::from_secs(5);
        let mut last_seen_block: Option<u64> = None;

        while !self.cancel_token.is_cancelled() {
            match self.fetch_latest_block().await {
                Err(e) => {
                    if self.cancel_token.is_cancelled() {
                        return;
                    }
                    error!(
                        error = %e,
                        backoff_ms = backoff.as_millis(),
                        "block watcher poll failed, retrying"
                    );

                    tokio::select! {
                        biased;
                        _ = self.cancel_token.cancelled() => return,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
                Ok(Some((latest_block, latest_hashes))) => {
                    backoff = Duration::from_millis(100);
                    let latest_block_number = latest_block.number;
                    let first_block = last_seen_block.map_or_else(
                        || latest_block_number.saturating_sub(INITIAL_BLOCK_LOOKBACK),
                        |block| block.saturating_add(1),
                    );

                    if first_block <= latest_block_number {
                        for block_number in first_block..=latest_block_number {
                            if self.cancel_token.is_cancelled() {
                                return;
                            }
                            trace!(block = block_number, "received new block");
                            let observed = if block_number == latest_block_number {
                                Some((latest_block, latest_hashes.clone()))
                            } else {
                                self.fetch_block(block_number)
                                    .await
                                    .inspect_err(|e| {
                                        warn!(
                                            block = block_number,
                                            error = %e,
                                            "failed to fetch block hashes"
                                        );
                                    })
                                    .ok()
                                    .flatten()
                            };
                            let Some((block, tx_hashes)) = observed else {
                                break;
                            };
                            self.results_tracker.on_new_block_hashes(block, tx_hashes);
                            last_seen_block = Some(block_number);
                        }
                    }
                }
                Ok(None) => {}
            }

            tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => return,
                _ = tokio::time::sleep(BLOCK_POLL_INTERVAL) => {}
            }
        }

        debug!("block watcher stopped");
    }

    async fn fetch_latest_block(
        &self,
    ) -> std::result::Result<Option<(BlockObservation, Vec<TxHash>)>, String> {
        self.fetch_block(BlockNumberOrTag::Latest).await
    }

    async fn fetch_block(
        &self,
        block: impl Into<BlockNumberOrTag>,
    ) -> std::result::Result<Option<(BlockObservation, Vec<TxHash>)>, String> {
        let observed_at = Instant::now();
        let block = tokio::time::timeout(BLOCK_RPC_TIMEOUT, async {
            self.provider.get_block_by_number(block.into()).hashes().await
        })
        .await
        .map_err(|_| format!("eth_getBlockByNumber timed out after {BLOCK_RPC_TIMEOUT:?}"))?
        .map_err(|e| e.to_string())?;

        let Some(block) = block else {
            return Ok(None);
        };

        let observation = BlockObservation {
            number: block.header.number,
            block_time: timestamp_to_instant(block.header.timestamp, observed_at),
            observed_at,
        };
        let tx_hashes = block.transactions.hashes().collect();

        Ok(Some((observation, tx_hashes)))
    }

    /// Fetches canonical receipts for the given block numbers in a single batch pass.
    ///
    /// Returns one [`BlockReceipt`] per transaction across all requested blocks.
    /// Intended for the end-of-run enrichment pass, where the caller already knows
    /// exactly which blocks contain its transactions, so receipts are fetched only
    /// for those blocks.
    pub async fn fetch_receipts(
        provider: &RootProvider<Base>,
        block_numbers: &[u64],
    ) -> Vec<BlockReceipt> {
        let mut out = Vec::new();
        for &block_number in block_numbers {
            let block_id = BlockId::Number(BlockNumberOrTag::Number(block_number));
            match tokio::time::timeout(BLOCK_RPC_TIMEOUT, provider.get_block_receipts(block_id))
                .await
            {
                Ok(Ok(Some(receipts))) => {
                    out.extend(receipts.into_iter().map(|receipt| BlockReceipt {
                        tx_hash: receipt.transaction_hash(),
                        block_number: receipt.block_number().unwrap_or(block_number),
                        gas_used: receipt.gas_used(),
                        effective_gas_price: receipt.effective_gas_price(),
                        success: receipt.status(),
                    }));
                }
                Ok(Ok(None)) => {
                    warn!(block = block_number, "eth_getBlockReceipts returned no receipts")
                }
                Ok(Err(e)) => {
                    warn!(block = block_number, error = %e, "eth_getBlockReceipts failed")
                }
                Err(_) => warn!(
                    block = block_number,
                    timeout_secs = BLOCK_RPC_TIMEOUT.as_secs(),
                    "eth_getBlockReceipts timed out"
                ),
            }
        }
        out
    }
}

fn timestamp_to_instant(timestamp: u64, now_instant: Instant) -> Option<Instant> {
    let now_system = SystemTime::now();
    let block_system = UNIX_EPOCH.checked_add(Duration::from_secs(timestamp))?;

    if let Ok(delta) = now_system.duration_since(block_system) {
        return now_instant.checked_sub(delta);
    }

    block_system.duration_since(now_system).ok().and_then(|delta| now_instant.checked_add(delta))
}
