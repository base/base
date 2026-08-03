//! Block watching and transaction landing detection via `eth_getBlockByNumber`.
//!
//! The watcher polls for new canonical blocks and reports the transaction hashes
//! contained in each block to the [`ResultsTracker`], which records landing latency.
//! Canonical receipts are sampled during the run for gas calibration and fetched again
//! in a complete end-of-run pass for final metrics.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_network::ReceiptResponse;
use alloy_primitives::TxHash;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::{BlockId, BlockNumberOrTag};
use base_common_network::Base;
use futures::{StreamExt, stream};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

use super::{BlockObservation, BlockReceipt, ResultsTracker};
use crate::utils::{BaselineError, Result};

/// How frequently to poll for a new canonical block.
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Maximum time to wait for a block watcher RPC request.
const BLOCK_RPC_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum time to wait for a block receipt RPC request.
const RECEIPT_RPC_TIMEOUT: Duration = Duration::from_secs(50);
/// Catch-up window when the first successful poll happens with no tip baseline.
///
/// If early polls fail (rate limits, RPC errors) while submissions are already
/// landing, a tiny lookback permanently misses those inclusions and every tx
/// eventually expires as "without confirmation". Sized for ~2s L2 blocks over
/// the pending-confirmation timeout (~200s) plus margin.
const CATCHUP_BLOCK_LOOKBACK: u64 = 256;
/// Maximum concurrent `eth_getBlockReceipts` requests during the end-of-run pass.
/// Blocks are independent, so they are fetched in parallel up to this bound.
const RECEIPT_FETCH_CONCURRENCY: usize = 3;

/// Set on the first `eth_getBlockReceipts` error response, so the headline "unavailable"
/// warning below fires once per process instead of once per failed block.
static BLOCK_RECEIPTS_UNAVAILABLE_WARNED: AtomicBool = AtomicBool::new(false);

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
        // Pin a tip before submissions matter whenever possible. Incremental scans
        // then start at tip+1. If this fails, the first successful poll uses
        // CATCHUP_BLOCK_LOOKBACK instead of a tiny window.
        let mut last_seen_block = self.establish_tip_baseline().await;
        let live_receipt_fetch = Arc::new(Semaphore::new(1));
        let mut last_progress_log = Instant::now();

        while !self.cancel_token.is_cancelled() {
            match self.fetch_latest_block().await {
                Err(e) => {
                    if self.cancel_token.is_cancelled() {
                        return;
                    }
                    error!(
                        error = %e,
                        backoff_ms = backoff.as_millis(),
                        pending = self.results_tracker.pending_count(),
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
                Ok(Some(latest)) => {
                    backoff = Duration::from_millis(100);
                    let latest_block_number = latest.0.number;
                    let first_block = last_seen_block.map_or_else(
                        || {
                            let start = latest_block_number.saturating_sub(CATCHUP_BLOCK_LOOKBACK);
                            warn!(
                                latest = latest_block_number,
                                start,
                                lookback = CATCHUP_BLOCK_LOOKBACK,
                                pending = self.results_tracker.pending_count(),
                                "block watcher has no tip baseline; catching up with large lookback"
                            );
                            start
                        },
                        |seen| seen.saturating_add(1),
                    );

                    if first_block <= latest_block_number {
                        if latest_block_number.saturating_sub(first_block) >= 32 {
                            info!(
                                from = first_block,
                                to = latest_block_number,
                                pending = self.results_tracker.pending_count(),
                                "block watcher catching up on canonical blocks"
                            );
                        }
                        // The latest block was already fetched above; move it out (no
                        // clone of its tx-hash Vec) on the final iteration that reaches
                        // it, and fetch only the intermediate gap blocks.
                        let mut latest = Some(latest);
                        let mut matched_total = 0u64;
                        for block_number in first_block..=latest_block_number {
                            if self.cancel_token.is_cancelled() {
                                return;
                            }
                            trace!(block = block_number, "received new block");
                            let observed = if block_number == latest_block_number {
                                latest.take()
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
                            let has_measured_pending =
                                self.results_tracker.has_measured_pending(&tx_hashes);
                            let matched =
                                self.results_tracker.on_new_block_hashes(block, tx_hashes);
                            matched_total = matched_total.saturating_add(matched);
                            if has_measured_pending {
                                if let Ok(permit) =
                                    Arc::clone(&live_receipt_fetch).try_acquire_owned()
                                {
                                    let provider = self.provider.clone();
                                    let results_tracker = self.results_tracker.clone();
                                    let cancel_token = self.cancel_token.clone();
                                    tokio::spawn(async move {
                                        tokio::select! {
                                            biased;
                                            _ = cancel_token.cancelled() => {}
                                            result = Self::fetch_block_receipts(
                                                &provider,
                                                block_number,
                                            ) => {
                                                results_tracker.observe_live_receipts(&result.0);
                                            }
                                        }
                                        drop(permit);
                                    });
                                } else {
                                    trace!(
                                        block = block_number,
                                        "skipping live receipts while previous fetch is pending"
                                    );
                                }
                            }
                            last_seen_block = Some(block_number);
                        }
                        if matched_total > 0 {
                            debug!(
                                matched = matched_total,
                                pending = self.results_tracker.pending_count(),
                                last_seen = ?last_seen_block,
                                "block watcher matched pending transactions"
                            );
                        } else if last_progress_log.elapsed() >= Duration::from_secs(15)
                            && self.results_tracker.pending_count() > 0
                        {
                            warn!(
                                pending = self.results_tracker.pending_count(),
                                last_seen = ?last_seen_block,
                                latest = latest_block_number,
                                "block watcher scanned blocks but matched no pending hashes"
                            );
                            last_progress_log = Instant::now();
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

    /// Fetches the current tip so incremental scans begin at tip+1 after startup.
    ///
    /// Returns `None` if the tip cannot be read before cancellation or the short
    /// startup budget expires; the main loop then falls back to
    /// [`CATCHUP_BLOCK_LOOKBACK`] on its first success.
    async fn establish_tip_baseline(&self) -> Option<u64> {
        let started = Instant::now();
        let budget = Duration::from_secs(3);
        let mut backoff = Duration::from_millis(100);

        while !self.cancel_token.is_cancelled() && started.elapsed() < budget {
            match self.fetch_latest_block().await {
                Ok(Some((block, _))) => {
                    info!(tip = block.number, "block watcher tip baseline established");
                    return Some(block.number);
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(
                        error = %e,
                        backoff_ms = backoff.as_millis(),
                        "block watcher tip baseline fetch failed, retrying"
                    );
                }
            }

            tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => return None,
                _ = tokio::time::sleep(backoff.min(budget.saturating_sub(started.elapsed()))) => {}
            }
            backoff = (backoff * 2).min(Duration::from_secs(1));
        }

        warn!("block watcher proceeding without tip baseline");
        None
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
        let Some((number, tx_hashes)) =
            Self::fetch_block_hashes(&self.provider, block.into()).await?
        else {
            return Ok(None);
        };

        Ok(Some((BlockObservation { number, observed_at }, tx_hashes)))
    }

    /// Waits until every hash in `pending` lands in a canonical block, then batch-fetches
    /// receipts for the touched blocks and returns them keyed by hash.
    ///
    /// Issues one `eth_getBlockReceipts` call per distinct block instead of one
    /// `eth_getTransactionReceipt` poll per transaction, so confirming thousands of setup
    /// or funding transactions costs a handful of RPC calls rather than thousands. Any hash
    /// the batch call didn't resolve — e.g. because the RPC doesn't implement
    /// `eth_getBlockReceipts` at all — falls back to an individual
    /// `eth_getTransactionReceipt`, so callers get a complete result as long as every hash
    /// landed in a block. A hash still missing from the returned map means even that
    /// fallback failed; callers should treat that as inconclusive, not as a revert.
    /// `on_landed` is invoked once per hash as it lands in a block (before receipts are
    /// fetched), so callers can drive a progress indicator.
    pub async fn confirm_and_fetch_receipts(
        provider: &RootProvider<Base>,
        mut pending: HashSet<TxHash>,
        timeout: Duration,
        mut on_landed: impl FnMut(TxHash),
    ) -> Result<HashMap<TxHash, BlockReceipt>> {
        let mut blocks_by_hash = HashMap::with_capacity(pending.len());
        Self::await_hashes(provider, &mut pending, timeout, |hash, block_number| {
            blocks_by_hash.insert(hash, block_number);
            on_landed(hash);
        })
        .await?;

        let block_numbers: Vec<u64> =
            blocks_by_hash.values().copied().collect::<HashSet<_>>().into_iter().collect();
        let (block_receipts, failed_blocks) = Self::fetch_receipts(provider, &block_numbers).await;
        let mut receipts: HashMap<TxHash, BlockReceipt> =
            block_receipts.into_iter().map(|receipt| (receipt.tx_hash, receipt)).collect();

        let missing: Vec<TxHash> =
            blocks_by_hash.keys().copied().filter(|hash| !receipts.contains_key(hash)).collect();
        if !missing.is_empty() {
            warn!(
                failed_blocks,
                total_blocks = block_numbers.len(),
                missing = missing.len(),
                "some transactions missing from batched block receipts; falling back to individual eth_getTransactionReceipt"
            );
            let fallback: Vec<(TxHash, Option<BlockReceipt>)> = stream::iter(missing)
                .map(|hash| Self::fetch_transaction_receipt(provider, hash))
                .buffer_unordered(RECEIPT_FETCH_CONCURRENCY)
                .collect()
                .await;
            for (hash, receipt) in fallback {
                if let Some(receipt) = receipt {
                    receipts.insert(hash, receipt);
                }
            }
        }

        Ok(receipts)
    }

    /// Fetches a single transaction's receipt directly, for hashes a batched
    /// `eth_getBlockReceipts` call didn't resolve (e.g. unsupported by the RPC).
    async fn fetch_transaction_receipt(
        provider: &RootProvider<Base>,
        tx_hash: TxHash,
    ) -> (TxHash, Option<BlockReceipt>) {
        let receipt =
            tokio::time::timeout(RECEIPT_RPC_TIMEOUT, provider.get_transaction_receipt(tx_hash))
                .await;
        let receipt = match receipt {
            Ok(Ok(Some(receipt))) => Some(BlockReceipt {
                tx_hash: receipt.transaction_hash(),
                block_number: receipt.block_number().unwrap_or_default(),
                gas_used: receipt.gas_used(),
                effective_gas_price: receipt.effective_gas_price(),
                success: receipt.status(),
            }),
            Ok(Ok(None)) => {
                warn!(tx_hash = %tx_hash, "eth_getTransactionReceipt returned no receipt");
                None
            }
            Ok(Err(e)) => {
                warn!(tx_hash = %tx_hash, error = %e, "eth_getTransactionReceipt failed");
                None
            }
            Err(_) => {
                warn!(
                    tx_hash = %tx_hash,
                    timeout_secs = RECEIPT_RPC_TIMEOUT.as_secs(),
                    "eth_getTransactionReceipt timed out"
                );
                None
            }
        };
        (tx_hash, receipt)
    }

    /// Waits until every hash in `pending` appears in a canonical block.
    ///
    /// Polls `eth_getBlockByNumber` (hash-only bodies) and checks hashes off as they
    /// land, instead of issuing one receipt/balance RPC per transaction. `on_confirmed`
    /// is invoked with the block number once per newly observed hash.
    pub async fn await_hashes(
        provider: &RootProvider<Base>,
        pending: &mut HashSet<TxHash>,
        timeout: Duration,
        mut on_confirmed: impl FnMut(TxHash, u64),
    ) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }

        let started = Instant::now();
        let initial_pending = pending.len();
        info!(
            pending = initial_pending,
            timeout_secs = timeout.as_secs(),
            "waiting for transaction hashes in canonical blocks"
        );
        let mut last_seen_block: Option<u64> = None;
        let mut last_progress_log = Instant::now();
        let mut backoff = Duration::from_millis(100);
        let max_backoff = Duration::from_secs(5);

        while !pending.is_empty() {
            if started.elapsed() >= timeout {
                return Err(BaselineError::Timeout {
                    operation: format!(
                        "confirming {} transaction hash(es) via block watcher",
                        pending.len()
                    ),
                    duration: timeout,
                });
            }

            match Self::fetch_block_hashes(provider, BlockNumberOrTag::Latest).await {
                Err(e) => {
                    warn!(
                        error = %e,
                        backoff_ms = backoff.as_millis(),
                        pending = pending.len(),
                        "block hash confirmation poll failed, retrying"
                    );
                    tokio::time::sleep(backoff.min(timeout.saturating_sub(started.elapsed())))
                        .await;
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
                Ok(None) => {}
                Ok(Some((latest_number, latest_hashes))) => {
                    backoff = Duration::from_millis(100);
                    let first_block = last_seen_block.map_or_else(
                        || latest_number.saturating_sub(CATCHUP_BLOCK_LOOKBACK),
                        |block| block.saturating_add(1),
                    );

                    if first_block <= latest_number {
                        let mut latest_hashes = Some(latest_hashes);
                        for block_number in first_block..=latest_number {
                            let hashes = if block_number == latest_number {
                                latest_hashes.take().unwrap_or_default()
                            } else {
                                match Self::fetch_block_hashes(
                                    provider,
                                    BlockNumberOrTag::Number(block_number),
                                )
                                .await
                                {
                                    Ok(Some((_, hashes))) => hashes,
                                    Ok(None) => break,
                                    Err(e) => {
                                        warn!(
                                            block = block_number,
                                            error = %e,
                                            "failed to fetch block hashes during confirmation"
                                        );
                                        break;
                                    }
                                }
                            };

                            for hash in hashes {
                                if pending.remove(&hash) {
                                    debug!(tx_hash = %hash, block = block_number, "transaction confirmed in block");
                                    on_confirmed(hash, block_number);
                                }
                            }
                            last_seen_block = Some(block_number);
                            if pending.is_empty() {
                                info!(
                                    confirmed = initial_pending,
                                    elapsed_ms = started.elapsed().as_millis() as u64,
                                    "all watched transaction hashes confirmed"
                                );
                                return Ok(());
                            }
                        }
                    }
                }
            }

            if last_progress_log.elapsed() >= Duration::from_secs(5) {
                info!(
                    remaining = pending.len(),
                    initial = initial_pending,
                    elapsed_secs = started.elapsed().as_secs(),
                    "still waiting for transaction hash confirmations"
                );
                last_progress_log = Instant::now();
            }

            tokio::time::sleep(BLOCK_POLL_INTERVAL.min(timeout.saturating_sub(started.elapsed())))
                .await;
        }

        Ok(())
    }

    async fn fetch_block_hashes(
        provider: &RootProvider<Base>,
        block: BlockNumberOrTag,
    ) -> std::result::Result<Option<(u64, Vec<TxHash>)>, String> {
        let block = tokio::time::timeout(BLOCK_RPC_TIMEOUT, async {
            provider.get_block_by_number(block).hashes().await
        })
        .await
        .map_err(|_| format!("eth_getBlockByNumber timed out after {BLOCK_RPC_TIMEOUT:?}"))?
        .map_err(|e| e.to_string())?;

        let Some(block) = block else {
            return Ok(None);
        };

        Ok(Some((block.header.number, block.transactions.hashes().collect())))
    }

    /// Fetches canonical receipts for the given block numbers in a single batch pass.
    ///
    /// Returns the [`BlockReceipt`]s for every transaction across all requested blocks,
    /// plus the count of blocks whose `eth_getBlockReceipts` call failed (timeout, RPC
    /// error, or missing receipts). A non-zero failed count means the receipt data is
    /// incomplete, so downstream gas/revert metrics are partial. Intended for the
    /// end-of-run enrichment pass, where the caller already knows exactly which blocks
    /// contain its transactions, so receipts are fetched only for those blocks.
    pub async fn fetch_receipts(
        provider: &RootProvider<Base>,
        block_numbers: &[u64],
    ) -> (Vec<BlockReceipt>, usize) {
        let per_block: Vec<(Vec<BlockReceipt>, bool)> = stream::iter(block_numbers.iter().copied())
            .map(|block_number| Self::fetch_block_receipts(provider, block_number))
            .buffer_unordered(RECEIPT_FETCH_CONCURRENCY)
            .collect()
            .await;

        let mut receipts = Vec::new();
        let mut failed_blocks = 0;
        for (block_receipts, failed) in per_block {
            if failed {
                failed_blocks += 1;
            }
            receipts.extend(block_receipts);
        }
        (receipts, failed_blocks)
    }

    /// Fetches the canonical receipts for a single block, mapping each into a
    /// [`BlockReceipt`]. On timeout, RPC error, or missing receipts (logged as a
    /// warning) returns `(empty vec, true)` so a single bad block cannot fail the pass
    /// while still being counted as a failed fetch by the caller.
    async fn fetch_block_receipts(
        provider: &RootProvider<Base>,
        block_number: u64,
    ) -> (Vec<BlockReceipt>, bool) {
        let block_id = BlockId::Number(BlockNumberOrTag::Number(block_number));
        match tokio::time::timeout(RECEIPT_RPC_TIMEOUT, provider.get_block_receipts(block_id)).await
        {
            Ok(Ok(Some(receipts))) => {
                let mapped = receipts
                    .into_iter()
                    .map(|receipt| BlockReceipt {
                        tx_hash: receipt.transaction_hash(),
                        block_number: receipt.block_number().unwrap_or(block_number),
                        gas_used: receipt.gas_used(),
                        effective_gas_price: receipt.effective_gas_price(),
                        success: receipt.status(),
                    })
                    .collect();
                (mapped, false)
            }
            Ok(Ok(None)) => {
                warn!(block = block_number, "eth_getBlockReceipts returned no receipts");
                (Vec::new(), true)
            }
            Ok(Err(e)) => {
                if !BLOCK_RECEIPTS_UNAVAILABLE_WARNED.swap(true, Ordering::Relaxed) {
                    warn!(
                        error = %e,
                        "eth_getBlockReceipts request failed; this RPC endpoint may not support \
                         batch block receipts, so receipt confirmation will fall back to slower \
                         per-transaction eth_getTransactionReceipt calls for the rest of this run"
                    );
                }
                warn!(block = block_number, error = %e, "eth_getBlockReceipts failed");
                (Vec::new(), true)
            }
            Err(_) => {
                warn!(
                    block = block_number,
                    timeout_secs = RECEIPT_RPC_TIMEOUT.as_secs(),
                    "eth_getBlockReceipts timed out"
                );
                (Vec::new(), true)
            }
        }
    }
}
