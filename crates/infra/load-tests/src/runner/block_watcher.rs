//! Block watching and transaction landing detection via `eth_getBlockByNumber`.
//!
//! The watcher polls for new canonical blocks and reports the transaction hashes
//! contained in each block to the [`ResultsTracker`], which records landing latency.
//! Canonical receipts are sampled during the run for gas calibration and fetched again
//! in a complete end-of-run pass for final metrics.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_network::ReceiptResponse;
use alloy_primitives::TxHash;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::{BlockId, BlockNumberOrTag};
use base_common_network::Base;
use futures::{StreamExt, stream};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::{BlockObservation, BlockReceipt, InclusionPulse, ResultsTracker};

/// Delay between probes while a scheduled block is becoming visible over RPC.
const BLOCK_AVAILABILITY_PROBE_INTERVAL: Duration = Duration::from_millis(10);
/// Maximum age of missed canonical blocks recovered after the newest pulse is published.
const LIVE_CATCHUP_WINDOW: Duration = Duration::from_secs(16);
/// Canonical blocks between live gas-calibration receipt samples.
const LIVE_RECEIPT_SAMPLE_BLOCKS: u64 = 10;
/// Maximum time to wait for a block watcher RPC request.
const BLOCK_RPC_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum time to wait for a block receipt RPC request.
const RECEIPT_RPC_TIMEOUT: Duration = Duration::from_secs(50);
/// Catch-up window when the first successful poll happens with no tip baseline.
///
/// If early polls fail (rate limits, RPC errors) while submissions are already
/// landing, a tiny lookback permanently misses those inclusions and every tx
/// eventually expires as "without confirmation". Covers the pending-confirmation
/// timeout (~200s) plus margin, preserving the legacy 256-block horizon at 2s.
const STARTUP_CATCHUP_WINDOW: Duration = Duration::from_secs(512);
/// Maximum concurrent `eth_getBlockReceipts` requests during the end-of-run pass.
/// Blocks are independent, so they are fetched in parallel up to this bound.
const RECEIPT_FETCH_CONCURRENCY: usize = 3;

/// Set on the first `eth_getBlockReceipts` error response, so the headline "unavailable"
/// warning below fires once per process instead of once per failed block.
static BLOCK_RECEIPTS_UNAVAILABLE_WARNED: AtomicBool = AtomicBool::new(false);

/// Timing and gas information emitted when a new canonical block becomes visible.
#[derive(Debug, Clone, Copy)]
pub struct BlockPulse {
    /// Canonical block number.
    pub number: u64,
    /// Gas consumed by the entire block.
    pub gas_used: u64,
    /// Block gas limit.
    pub gas_limit: u64,
    /// Block base fee in wei.
    pub base_fee: u128,
    /// Gas limit of this load test's transactions matched in the block.
    pub our_included_gas: u128,
    /// Predicted canonical boundary for this block.
    pub expected_boundary: Instant,
    /// Time at which the RPC response became visible to the load tester.
    pub observed_at: Instant,
}

/// Phase-locked schedule for probing canonical block availability.
#[derive(Debug, Clone, Copy)]
pub struct BlockClock {
    block_time: Duration,
    expected_boundary: Instant,
}

impl BlockClock {
    /// Creates a clock aligned to the boundary after `block_timestamp_ms`.
    pub fn from_block_timestamp_ms(
        block_time: Duration,
        block_timestamp_ms: u64,
        system_now: SystemTime,
        instant_now: Instant,
    ) -> Self {
        let block_timestamp = Duration::from_millis(block_timestamp_ms);
        let system_elapsed = system_now.duration_since(UNIX_EPOCH).unwrap_or_default();
        if block_timestamp > system_elapsed.saturating_add(block_time) {
            return Self::from_now(block_time, instant_now);
        }
        let mut next_elapsed = block_timestamp.saturating_add(block_time);
        while next_elapsed.saturating_add(block_time) <= system_elapsed {
            next_elapsed = next_elapsed.saturating_add(block_time);
        }
        let expected_boundary = if next_elapsed >= system_elapsed {
            instant_now + next_elapsed.saturating_sub(system_elapsed)
        } else {
            instant_now
                .checked_sub(system_elapsed.saturating_sub(next_elapsed))
                .unwrap_or(instant_now)
        };
        Self { block_time, expected_boundary }
    }

    /// Creates an unaligned clock that probes after one configured interval.
    pub fn from_now(block_time: Duration, now: Instant) -> Self {
        Self { block_time, expected_boundary: now + block_time }
    }

    /// Returns the next predicted canonical boundary.
    pub const fn expected_boundary(&self) -> Instant {
        self.expected_boundary
    }

    /// Advances by the number of newly observed blocks without accumulating RPC delay.
    pub fn advance(&mut self, blocks: u64) {
        let blocks = u32::try_from(blocks.max(1)).unwrap_or(u32::MAX);
        self.expected_boundary += self.block_time.saturating_mul(blocks);
    }

    /// Applies one small earlier phase correction after an immediately available block.
    pub fn correct_earlier(&mut self) {
        let correction = (self.block_time / 8).min(Duration::from_millis(25));
        self.expected_boundary =
            self.expected_boundary.checked_sub(correction).unwrap_or(self.expected_boundary);
    }

    /// Returns the window in which rapid availability probes are expected.
    pub fn availability_window(&self) -> Duration {
        (self.block_time / 2).clamp(Duration::from_millis(1), Duration::from_millis(250))
    }
}

#[derive(Debug)]
struct ObservedBlock {
    observation: BlockObservation,
    tx_hashes: Vec<TxHash>,
    gas_used: u64,
    gas_limit: u64,
    base_fee: u128,
    timestamp_ms: u64,
}

/// Polls canonical blocks and reports their transaction hashes for landing detection.
#[derive(Debug)]
pub struct BlockWatcher {
    provider: RootProvider<Base>,
    results_tracker: ResultsTracker,
    block_time: Duration,
    pulse_tx: mpsc::Sender<InclusionPulse>,
    cancel_token: CancellationToken,
}

impl BlockWatcher {
    /// Creates a new [`BlockWatcher`].
    pub const fn new(
        provider: RootProvider<Base>,
        results_tracker: ResultsTracker,
        block_time: Duration,
        pulse_tx: mpsc::Sender<InclusionPulse>,
        cancel_token: CancellationToken,
    ) -> Self {
        Self { provider, results_tracker, block_time, pulse_tx, cancel_token }
    }

    /// Spawns the watcher as a background task.
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    async fn run(&self) {
        info!("started block watcher");

        let baseline = self.establish_tip_baseline().await;
        let mut last_seen_block = baseline.as_ref().map(|block| block.observation.number);
        let mut clock = baseline.as_ref().map_or_else(
            || BlockClock::from_now(self.block_time, Instant::now()),
            |block| {
                BlockClock::from_block_timestamp_ms(
                    self.block_time,
                    block.timestamp_ms,
                    SystemTime::now(),
                    Instant::now(),
                )
            },
        );
        let live_receipt_fetch = Arc::new(Semaphore::new(1));
        let mut last_progress_log = Instant::now();
        let mut last_availability_warning =
            Instant::now().checked_sub(Duration::from_secs(15)).unwrap_or_else(Instant::now);
        let mut consecutive_probe_failures = 0u64;
        let mut last_probe_warning =
            Instant::now().checked_sub(Duration::from_secs(15)).unwrap_or_else(Instant::now);

        while !self.cancel_token.is_cancelled() {
            tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => return,
                _ = tokio::time::sleep_until(clock.expected_boundary().into()) => {}
            }

            let expected_boundary = clock.expected_boundary();
            let availability_deadline = expected_boundary + clock.availability_window();
            let mut availability_miss_logged = false;
            let mut required_retry = false;
            let latest = loop {
                match self.fetch_latest_block_with_timeout(BLOCK_RPC_TIMEOUT).await {
                    Ok(block) => {
                        if let Some(block) = block
                            && last_seen_block.is_none_or(|seen| block.observation.number > seen)
                        {
                            if consecutive_probe_failures > 0 {
                                debug!(
                                    failed_probes = consecutive_probe_failures,
                                    "block availability probes recovered"
                                );
                                consecutive_probe_failures = 0;
                            }
                            break Some(block);
                        }
                        required_retry = true;
                    }
                    Err(error) => {
                        required_retry = true;
                        consecutive_probe_failures = consecutive_probe_failures.saturating_add(1);
                        if last_probe_warning.elapsed() >= Duration::from_secs(15) {
                            warn!(
                                error = %error,
                                failed_probes = consecutive_probe_failures,
                                pending = self.results_tracker.pending_count(),
                                "block availability probes failing"
                            );
                            last_probe_warning = Instant::now();
                        }
                    }
                }

                if !availability_miss_logged && Instant::now() >= availability_deadline {
                    if last_availability_warning.elapsed() >= Duration::from_secs(15) {
                        warn!(
                            expected_boundary_ms_ago = Instant::now()
                                .saturating_duration_since(expected_boundary)
                                .as_millis(),
                            "canonical block missed configured availability window"
                        );
                        last_availability_warning = Instant::now();
                    }
                    availability_miss_logged = true;
                }
                tokio::select! {
                    biased;
                    _ = self.cancel_token.cancelled() => return,
                    _ = tokio::time::sleep(if availability_miss_logged {
                        (self.block_time / 4)
                            .clamp(Duration::from_millis(1), Duration::from_millis(100))
                    } else {
                        BLOCK_AVAILABILITY_PROBE_INTERVAL
                    }) => {}
                }
            };

            let Some(latest) = latest else {
                continue;
            };
            let latest_number = latest.observation.number;
            let latest_tx_count = latest.tx_hashes.len();
            let blocks_advanced =
                last_seen_block.map_or(1, |seen| latest_number.saturating_sub(seen).max(1));
            let pulse_expected_boundary = expected_boundary
                + self.block_time.saturating_mul(
                    u32::try_from(blocks_advanced.saturating_sub(1)).unwrap_or(u32::MAX),
                );
            if !availability_miss_logged
                && latest.observation.observed_at
                    >= pulse_expected_boundary + clock.availability_window()
                && last_availability_warning.elapsed() >= Duration::from_secs(15)
            {
                warn!(
                    availability_lag_ms = latest
                        .observation
                        .observed_at
                        .saturating_duration_since(pulse_expected_boundary)
                        .as_millis(),
                    "canonical block exceeded configured availability window"
                );
                last_availability_warning = Instant::now();
            }
            clock.advance(blocks_advanced);
            if !required_retry {
                clock.correct_earlier();
            }

            // Process and publish the newest block first. Confirmation recovery for any
            // missed intermediate blocks must not delay refilling for the next block.
            let has_measured_pending = self.results_tracker.has_measured_pending(&latest.tx_hashes);
            let block_match =
                self.results_tracker.on_new_block_hashes(latest.observation, latest.tx_hashes);
            let block_pulse = BlockPulse {
                number: latest_number,
                gas_used: latest.gas_used,
                gas_limit: latest.gas_limit,
                base_fee: latest.base_fee,
                our_included_gas: block_match.included_gas,
                expected_boundary: pulse_expected_boundary,
                observed_at: latest.observation.observed_at,
            };
            if self
                .pulse_tx
                .send(InclusionPulse::canonical(block_pulse, block_match.released_gas))
                .await
                .is_err()
            {
                return;
            }

            if has_measured_pending
                && latest_number.is_multiple_of(LIVE_RECEIPT_SAMPLE_BLOCKS)
                && let Ok(permit) = Arc::clone(&live_receipt_fetch).try_acquire_owned()
            {
                let provider = self.provider.clone();
                let results_tracker = self.results_tracker.clone();
                let cancel_token = self.cancel_token.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        biased;
                        _ = cancel_token.cancelled() => {}
                        result = Self::fetch_block_receipts(&provider, latest_number) => {
                            results_tracker.observe_live_receipts(&result.0);
                        }
                    }
                    drop(permit);
                });
            }

            if block_match.matched > 0 {
                debug!(
                    matched = block_match.matched,
                    included_gas = block_match.included_gas,
                    pending = self.results_tracker.pending_count(),
                    block = latest_number,
                    "block watcher matched pending transactions"
                );
            } else if last_progress_log.elapsed() >= Duration::from_secs(15)
                && self.results_tracker.pending_count() > 0
            {
                let (measured_pending, measured_pending_gas) =
                    self.results_tracker.measured_unconfirmed_inventory();
                warn!(
                    pending = self.results_tracker.pending_count(),
                    in_flight = self.results_tracker.total_in_flight(),
                    measured_pending,
                    measured_pending_gas,
                    latest = latest_number,
                    latest_txs = latest_tx_count,
                    latest_gas_used = latest.gas_used,
                    latest_gas_limit = latest.gas_limit,
                    blocks_advanced,
                    block_has_measured_pending = has_measured_pending,
                    "block watcher scanned block but matched no pending hashes"
                );
                last_progress_log = Instant::now();
            }

            let catchup_first = match last_seen_block {
                Some(previous) if latest_number > previous.saturating_add(1) => {
                    Some(previous.saturating_add(1).max(latest_number.saturating_sub(
                        Self::catchup_blocks(LIVE_CATCHUP_WINDOW, self.block_time),
                    )))
                }
                None if self.results_tracker.pending_count() > 0 => {
                    Some(latest_number.saturating_sub(Self::catchup_blocks(
                        STARTUP_CATCHUP_WINDOW,
                        self.block_time,
                    )))
                }
                _ => None,
            };
            if let Some(first) = catchup_first {
                let last = latest_number.saturating_sub(1);
                let provider = self.provider.clone();
                let results_tracker = self.results_tracker.clone();
                let cancel_token = self.cancel_token.clone();
                tokio::spawn(async move {
                    for block_number in first..=last {
                        let result = tokio::select! {
                            biased;
                            () = cancel_token.cancelled() => break,
                            result = Self::fetch_block_hashes(
                                &provider,
                                BlockNumberOrTag::Number(block_number),
                            ) => result,
                        };
                        match result {
                            Ok(Some((number, hashes))) => {
                                results_tracker.on_new_block_hashes(
                                    BlockObservation { number, observed_at: Instant::now() },
                                    hashes,
                                );
                            }
                            Ok(None) => {}
                            Err(error) => {
                                warn!(
                                    block = block_number,
                                    error = %error,
                                    "failed to recover skipped canonical block"
                                );
                            }
                        }
                    }
                });
            }
            last_seen_block = Some(latest_number);
        }

        debug!("block watcher stopped");
    }

    /// Fetches the current tip so incremental scans begin at tip+1 after startup.
    ///
    /// Returns `None` if the tip cannot be read before cancellation or the short
    /// startup budget expires; the main loop then falls back to
    /// [`STARTUP_CATCHUP_WINDOW`] on its first success.
    async fn establish_tip_baseline(&self) -> Option<ObservedBlock> {
        let started = Instant::now();
        let budget = Duration::from_secs(3);
        let mut backoff = Duration::from_millis(100);

        while !self.cancel_token.is_cancelled() && started.elapsed() < budget {
            match self.fetch_latest_block().await {
                Ok(Some(block)) => {
                    info!(tip = block.observation.number, "block watcher tip baseline established");
                    return Some(block);
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

    async fn fetch_latest_block(&self) -> std::result::Result<Option<ObservedBlock>, String> {
        self.fetch_block(BlockNumberOrTag::Latest, BLOCK_RPC_TIMEOUT).await
    }

    async fn fetch_latest_block_with_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<Option<ObservedBlock>, String> {
        self.fetch_block(BlockNumberOrTag::Latest, timeout).await
    }

    async fn fetch_block(
        &self,
        block: impl Into<BlockNumberOrTag>,
        timeout: Duration,
    ) -> std::result::Result<Option<ObservedBlock>, String> {
        let block = tokio::time::timeout(timeout, async {
            self.provider.get_block_by_number(block.into()).hashes().await
        })
        .await
        .map_err(|_| format!("eth_getBlockByNumber timed out after {timeout:?}"))?
        .map_err(|e| e.to_string())?;
        let Some(block) = block else {
            return Ok(None);
        };
        let observed_at = Instant::now();

        Ok(Some(ObservedBlock {
            observation: BlockObservation { number: block.header.number, observed_at },
            tx_hashes: block.transactions.hashes().collect(),
            gas_used: block.header.gas_used,
            gas_limit: block.header.gas_limit,
            base_fee: u128::from(block.header.base_fee_per_gas.unwrap_or_default()),
            timestamp_ms: Self::block_timestamp_ms(&block),
        }))
    }

    /// Returns the full RPC millisecond timestamp, or derives it from legacy seconds.
    pub fn block_timestamp_ms(block: &<Base as alloy_network::Network>::BlockResponse) -> u64 {
        block.header.timestamp_ms.unwrap_or_else(|| block.header.timestamp.saturating_mul(1_000))
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

    /// Converts a catch-up duration to a block count, rounding partial blocks up.
    pub fn catchup_blocks(window: Duration, block_time: Duration) -> u64 {
        assert!(!block_time.is_zero(), "block time must be greater than zero");
        u64::try_from(window.as_nanos().div_ceil(block_time.as_nanos())).unwrap_or(u64::MAX)
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
                debug!(block = block_number, "eth_getBlockReceipts returned no receipts");
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
                debug!(block = block_number, error = %e, "eth_getBlockReceipts failed");
                (Vec::new(), true)
            }
            Err(_) => {
                debug!(
                    block = block_number,
                    timeout_secs = RECEIPT_RPC_TIMEOUT.as_secs(),
                    "eth_getBlockReceipts timed out"
                );
                (Vec::new(), true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc_block(
        timestamp: u64,
        timestamp_ms: Option<u64>,
    ) -> <Base as alloy_network::Network>::BlockResponse {
        let block = <Base as alloy_network::Network>::BlockResponse::default();
        let mut json = serde_json::to_value(block).unwrap();
        json["timestamp"] = serde_json::json!(format!("0x{timestamp:x}"));
        if let Some(timestamp_ms) = timestamp_ms {
            json["timestampMs"] = serde_json::json!(format!("0x{timestamp_ms:x}"));
        }
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn rpc_block_uses_full_millisecond_timestamp() {
        let block = rpc_block(100, Some(100_800));

        assert_eq!(BlockWatcher::block_timestamp_ms(&block), 100_800);
    }

    #[test]
    fn rpc_block_falls_back_to_legacy_second_timestamp() {
        let block = rpc_block(100, None);

        assert_eq!(BlockWatcher::block_timestamp_ms(&block), 100_000);
    }

    #[test]
    fn clock_preserves_millisecond_timestamp_across_second_boundary() {
        let instant_now = Instant::now();
        let clock = BlockClock::from_block_timestamp_ms(
            Duration::from_millis(200),
            100_800,
            UNIX_EPOCH + Duration::from_millis(100_850),
            instant_now,
        );

        assert_eq!(
            clock.expected_boundary().duration_since(instant_now),
            Duration::from_millis(150)
        );
    }

    #[test]
    fn clock_keeps_recent_boundary_in_the_past_for_immediate_probe() {
        let instant_now = Instant::now();
        let system_now = UNIX_EPOCH + Duration::from_millis(100_100);

        let clock = BlockClock::from_block_timestamp_ms(
            Duration::from_secs(2),
            98_000,
            system_now,
            instant_now,
        );

        assert_eq!(
            instant_now.saturating_duration_since(clock.expected_boundary()),
            Duration::from_millis(100)
        );
    }

    #[test]
    fn clock_uses_monotonic_interval_when_chain_timestamp_is_far_ahead() {
        let instant_now = Instant::now();
        let block_time = Duration::from_millis(200);
        let system_now = UNIX_EPOCH + Duration::from_millis(100_100);

        let clock =
            BlockClock::from_block_timestamp_ms(block_time, 105_000, system_now, instant_now);

        assert_eq!(clock.expected_boundary().duration_since(instant_now), block_time);
    }

    #[test]
    fn advancing_clock_does_not_accumulate_rpc_delay() {
        let now = Instant::now();
        let mut clock = BlockClock::from_now(Duration::from_secs(2), now);
        let first = clock.expected_boundary();

        clock.advance(1);

        assert_eq!(clock.expected_boundary().duration_since(first), Duration::from_secs(2));
    }

    #[test]
    fn availability_window_is_bounded() {
        let fast = BlockClock::from_now(Duration::from_millis(200), Instant::now());
        let normal = BlockClock::from_now(Duration::from_secs(2), Instant::now());

        assert_eq!(fast.availability_window(), Duration::from_millis(100));
        assert_eq!(normal.availability_window(), Duration::from_millis(250));
    }

    #[test]
    fn phase_correction_is_small_and_bounded() {
        let now = Instant::now();
        let mut clock = BlockClock::from_now(Duration::from_millis(200), now);
        let before = clock.expected_boundary();

        clock.correct_earlier();

        assert_eq!(before.duration_since(clock.expected_boundary()), Duration::from_millis(25));
    }

    #[test]
    fn catchup_windows_preserve_two_second_behavior_and_scale_for_denim() {
        assert_eq!(BlockWatcher::catchup_blocks(LIVE_CATCHUP_WINDOW, Duration::from_secs(2)), 8);
        assert_eq!(
            BlockWatcher::catchup_blocks(STARTUP_CATCHUP_WINDOW, Duration::from_secs(2)),
            256
        );
        assert_eq!(
            BlockWatcher::catchup_blocks(LIVE_CATCHUP_WINDOW, Duration::from_millis(200)),
            80
        );
        assert_eq!(
            BlockWatcher::catchup_blocks(STARTUP_CATCHUP_WINDOW, Duration::from_millis(200)),
            2_560
        );
    }

    #[test]
    fn catchup_blocks_rounds_partial_intervals_up() {
        assert_eq!(
            BlockWatcher::catchup_blocks(Duration::from_millis(201), Duration::from_millis(200)),
            2
        );
    }

    #[test]
    #[should_panic(expected = "block time must be greater than zero")]
    fn catchup_blocks_rejects_zero_block_time() {
        BlockWatcher::catchup_blocks(Duration::from_secs(1), Duration::ZERO);
    }
}
