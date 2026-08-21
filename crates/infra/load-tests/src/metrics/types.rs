use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use alloy_primitives::TxHash;
use serde::{Deserialize, Serialize};

/// Submission outcome counts collected during a load test, passed as a single
/// input bundle to `MetricsAggregator::summarize`.
#[derive(Debug, Clone, Copy)]
pub struct SubmissionStats<'a> {
    /// Total transactions submitted.
    pub submitted: u64,
    /// Total transactions that failed (e.g. rejected, expired without
    /// confirmation).
    pub failed: u64,
    /// Failure reason counts, used to surface the top-N reasons in the summary.
    pub failure_reasons: &'a HashMap<String, u64>,
}

/// Submission cohort a transaction was routed through, in serialized output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmitCohortLabel {
    /// Plain `eth_sendRawTransaction` submission carrying no predicates.
    #[default]
    Plain,
    /// Validity submission carrying resolved predicates.
    ValidityPass,
}

/// Metrics for a single transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionMetrics {
    /// Transaction hash.
    pub tx_hash: TxHash,
    /// Time from submission to first observation in a polled block (includes the
    /// block poll + scan cost).
    pub block_latency: Option<Duration>,
    /// Time from submission to sequencer acceptance.
    pub flashblocks_latency: Option<Duration>,
    /// Gas used by the transaction.
    pub gas_used: u64,
    /// Gas price in wei.
    pub gas_price: u128,
    /// Block number where transaction was included.
    pub block_number: Option<u64>,
    /// Whether the transaction reverted during execution.
    pub reverted: bool,
    /// Submission cohort this transaction was routed through.
    #[serde(default)]
    pub cohort: SubmitCohortLabel,
    /// When canonical inclusion was observed (used by the rolling window).
    #[serde(skip)]
    pub confirmed_at: Option<Instant>,
}

impl TransactionMetrics {
    /// Creates new transaction metrics.
    pub const fn new(
        tx_hash: TxHash,
        block_latency: Option<Duration>,
        flashblocks_latency: Option<Duration>,
        gas_used: u64,
        gas_price: u128,
        block_number: Option<u64>,
    ) -> Self {
        Self {
            tx_hash,
            block_latency,
            flashblocks_latency,
            gas_used,
            gas_price,
            block_number,
            reverted: false,
            cohort: SubmitCohortLabel::Plain,
            confirmed_at: None,
        }
    }

    /// Returns the transaction cost in wei.
    pub const fn cost_wei(&self) -> u128 {
        self.gas_used as u128 * self.gas_price
    }
}

/// Aggregated latency metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LatencyMetrics {
    /// Minimum latency observed.
    pub min: Duration,
    /// Maximum latency observed.
    pub max: Duration,
    /// Mean latency.
    pub mean: Duration,
    /// Median latency (p50).
    pub p50: Duration,
    /// 95th percentile latency.
    pub p95: Duration,
    /// 99th percentile latency.
    pub p99: Duration,
}

/// Confirmed-transaction metrics broken down for a single submission cohort.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CohortMetrics {
    /// Cohort these metrics summarize.
    pub cohort: SubmitCohortLabel,
    /// Confirmed transactions in this cohort.
    pub confirmed: u64,
    /// Confirmed transactions in this cohort that reverted during execution.
    pub reverted: u64,
    /// Total gas used by confirmed transactions in this cohort.
    pub total_gas: u64,
    /// Block landing latency for this cohort's confirmed transactions.
    pub block_latency: LatencyMetrics,
}

/// Aggregated throughput metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThroughputMetrics {
    /// Total transactions submitted.
    pub total_submitted: u64,
    /// Total transactions confirmed.
    pub total_confirmed: u64,
    /// Total transactions failed.
    pub total_failed: u64,
    /// Total confirmed transactions that reverted during execution.
    pub total_reverted: u64,
    /// Transactions per second achieved.
    pub tps: f64,
    /// Gas per second achieved.
    pub gps: f64,
    /// Total duration of the test.
    pub duration: Duration,
}

impl ThroughputMetrics {
    /// Returns the success rate (confirmed / submitted) as a percentage.
    pub fn success_rate(&self) -> f64 {
        if self.total_submitted == 0 {
            return 0.0;
        }
        (self.total_confirmed as f64 / self.total_submitted as f64) * 100.0
    }
}

/// Rolling-window throughput percentiles sampled during the run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThroughputPercentiles {
    /// Median rolling TPS.
    pub tps_p50: f64,
    /// 90th percentile rolling TPS.
    pub tps_p90: f64,
    /// 99th percentile rolling TPS.
    pub tps_p99: f64,
    /// Peak rolling TPS observed.
    pub tps_max: f64,
    /// Median rolling GPS.
    pub gps_p50: f64,
    /// 90th percentile rolling GPS.
    pub gps_p90: f64,
    /// 99th percentile rolling GPS.
    pub gps_p99: f64,
    /// Peak rolling GPS observed.
    pub gps_max: f64,
}

/// Aggregated gas metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GasMetrics {
    /// Total gas used.
    pub total_gas: u64,
    /// Average gas per transaction.
    pub avg_gas: u64,
    /// Total cost in wei.
    pub total_cost_wei: u128,
    /// Average gas price in wei.
    pub avg_gas_price: u128,
}

/// A single throughput sample captured during the test run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThroughputSample {
    /// Elapsed time since the test started, in seconds.
    pub elapsed_secs: f64,
    /// Rolling 30s transactions-per-second at this point.
    pub tps: f64,
    /// Rolling 30s gas-per-second at this point.
    pub gps: f64,
}

/// Source that triggered a pacing cycle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PacingCycleSource {
    /// Canonical block polling.
    #[default]
    Canonical,
    /// Builder flashblock broadcast.
    Flashblock,
    /// Timer fallback.
    Safety,
}

/// One block-aligned pacing cycle recorded by the runner.
#[derive(Debug, Clone, Copy, Default)]
pub struct PacingCycleObservation {
    /// Inclusion source that triggered this cycle.
    pub source: PacingCycleSource,
    /// Elapsed measured-run time when this cycle started.
    pub elapsed: Duration,
    /// Whether the cycle was triggered by a newly observed canonical block.
    pub block_observed: bool,
    /// Total gas used by the observed block.
    pub block_gas_used: u64,
    /// Gas limit of the observed block.
    pub block_gas_limit: u64,
    /// Estimated execution gas from this load test included in the observed block.
    pub our_included_gas: u128,
    /// Estimated mempool depth before the refill cycle.
    pub pre_refill_depth_gas: u128,
    /// Estimated mempool depth after the refill cycle.
    pub post_refill_depth_gas: u128,
    /// Estimated gas waiting in the local submission pipeline.
    pub queued_gas: u128,
    /// Desired one-block floor.
    pub floor_gas: u128,
    /// Estimated execution gas selected for submission.
    pub offered_gas: u128,
    /// Whether sender or global transaction capacity limited the plan.
    pub capacity_limited: bool,
    /// Whether the cycle began at the controller's depth ceiling.
    pub chain_bound: bool,
    /// Whether the presign buffer could not supply the planned gas.
    pub presign_starved: bool,
    /// Canonical boundary to block availability.
    pub availability_lag: Option<Duration>,
    /// Controller planning and presign-buffer selection time.
    pub plan_time: Duration,
    /// Refill batch enqueue-to-terminal-acknowledgement time.
    pub submit_time: Option<Duration>,
    /// Inclusion observation to terminal acknowledgement of all refill batches.
    pub refill_lag: Option<Duration>,
}

/// Aggregate health of block-aligned mempool pacing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PacingMetrics {
    /// Estimated execution gas offered by measured refill cycles.
    pub offered_gas: u128,
    /// Offered gas per wall-clock second.
    pub offered_gps: f64,
    /// Canonical blocks observed by the pacing loop.
    pub blocks_observed: u64,
    /// Refill cycles triggered by canonical block polling.
    pub canonical_cycles: u64,
    /// Refill cycles triggered by flashblock inclusion.
    pub flashblock_cycles: u64,
    /// Timer-driven fallback cycles.
    pub safety_cycles: u64,
    /// Observed blocks whose pre-refill depth was below the one-block floor.
    pub blocks_under_floor: u64,
    /// Cycles limited by sender in-flight capacity.
    pub capacity_limited_cycles: u64,
    /// Cycles starved by the presign buffer.
    pub presign_starved_cycles: u64,
    /// Refill cycles whose acknowledgement exceeded the 100ms budget.
    pub rpc_bound_cycles: u64,
    /// Block cycles that began at the two-block ceiling.
    pub chain_bound_cycles: u64,
    /// Maximum estimated submitted-but-unconfirmed gas.
    pub max_depth_gas: u128,
    /// Maximum estimated gas waiting in the local submission pipeline.
    pub max_queued_gas: u128,
    /// Mean ratio of estimated depth to the configured one-block floor.
    pub mean_depth_to_floor_ratio: f64,
    /// Mean total block fill ratio.
    pub mean_block_fill_ratio: f64,
    /// Mean fraction of each block gas limit consumed by estimated load-test execution gas.
    pub mean_our_block_ratio: f64,
    /// Canonical boundary-to-availability latency.
    pub availability_lag: LatencyMetrics,
    /// Controller plan latency.
    pub plan_time: LatencyMetrics,
    /// Submission acknowledgement latency after planning.
    pub submit_time: LatencyMetrics,
    /// Inclusion observation-to-refill acknowledgement latency.
    pub refill_lag: LatencyMetrics,
    /// Submitted transactions intentionally left undrained at cutoff.
    pub undrained_transactions: u64,
    /// Estimated gas intentionally left undrained at cutoff.
    pub undrained_gas: u128,
}

/// Range of block numbers in which test transactions were included.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockRange {
    /// First block containing a confirmed test transaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_block: Option<u64>,
    /// Last block containing a confirmed test transaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_block: Option<u64>,
    /// Inclusive number of blocks spanned (`last_block - first_block + 1`),
    /// or `0` when no test transactions were confirmed.
    pub block_count: u64,
}

impl BlockRange {
    /// Returns the duration spanned by this block range using `block_time`,
    /// or `None` when the range spans fewer than 2 blocks.
    pub fn block_time_duration(&self, block_time: Duration) -> Option<Duration> {
        if self.block_count < 2 {
            return None;
        }
        Some(block_time * (self.block_count - 1) as u32)
    }
}

/// Aggregated load-test transaction density for one L2 block.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockLoadMetrics {
    /// L2 block number.
    pub block_number: u64,
    /// Confirmed load-test transactions in this block.
    pub confirmed_count: u64,
    /// Total gas used by confirmed load-test transactions in this block.
    pub total_gas: u64,
}

/// Aggregated flashblocks latency percentiles.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FlashblocksLatencyMetrics {
    /// Transactions with flashblocks data.
    pub count: u64,
    /// Minimum latency observed.
    pub min: Duration,
    /// Maximum latency observed.
    pub max: Duration,
    /// Mean latency.
    pub mean: Duration,
    /// Median latency.
    pub p50: Duration,
    /// 90th percentile latency.
    pub p90: Duration,
    /// 95th percentile latency.
    pub p95: Duration,
    /// 99th percentile latency.
    pub p99: Duration,
}

/// Test configuration included in the JSON output (excludes URLs and secrets).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSummary {
    /// Amount funded to each sender account (in wei, as string).
    pub funding_amount: String,
    /// Number of sender accounts.
    pub sender_count: u32,
    /// Offset into the derivation path.
    pub sender_offset: u32,
    /// Maximum in-flight transactions per sender.
    pub in_flight_per_sender: u32,
    /// Optional ceiling on total in-flight transactions across all senders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_in_flight: Option<u32>,
    /// Optional cap on concurrent outbound submission RPC requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_submit_requests: Option<u32>,
    /// Maximum number of transactions in each JSON-RPC batch request.
    #[serde(default)]
    pub batch_size: u32,
    /// Test duration.
    pub duration: Option<String>,
    /// Optional gas-per-second target used to size the per-block mempool floor.
    pub target_gps: Option<u64>,
    /// Expected cadence between canonical blocks.
    pub block_time: String,
    /// Deterministic account seed.
    pub seed: u64,
    /// Chain ID.
    pub chain_id: Option<u64>,
    /// Transaction type configuration.
    pub transactions: serde_json::Value,
    /// Fraction of transactions targeting freshly derived recipient addresses.
    #[serde(default)]
    pub fresh_recipient_ratio: f64,
    /// Fraction of senders routed through the validity submission endpoint.
    #[serde(default)]
    pub validity_ratio: f64,
    /// Number of predicate templates attached to validity transactions.
    #[serde(default)]
    pub validity_predicate_count: usize,
    /// Address of the precompile looper contract.
    pub looper_contract: Option<String>,
    /// Amount of each swap token per sender (in wei, as string).
    pub swap_token_amount: String,
    /// Amount of B-20 tokens to mint per sender (in wei, as string).
    pub b20_mint_amount: String,

    /// Real-token setup configuration, when enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub real_token_setup: Option<serde_json::Value>,
}
