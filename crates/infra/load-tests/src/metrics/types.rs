use std::time::{Duration, Instant};

use alloy_primitives::TxHash;
use serde::{Deserialize, Serialize};

/// Metrics for a single transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionMetrics {
    /// Transaction hash.
    pub tx_hash: TxHash,
    /// Time from submission to block production.
    pub block_latency: Option<Duration>,
    /// Time from block production to receipt observation by the block watcher.
    pub block_receipt_delay: Option<Duration>,
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
            block_receipt_delay: None,
            flashblocks_latency,
            gas_used,
            gas_price,
            block_number,
            reverted: false,
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
    /// Number of transactions per RPC batch.
    pub batch_size: u32,
    /// Maximum wait before flushing a partial batch.
    pub batch_timeout: Option<String>,
    /// Test duration.
    pub duration: Option<String>,
    /// Target gas per second.
    pub target_gps: Option<u64>,
    /// Deterministic account seed.
    pub seed: u64,
    /// Chain ID.
    pub chain_id: Option<u64>,
    /// Transaction type configuration.
    pub transactions: serde_json::Value,
    /// Address of the precompile looper contract.
    pub looper_contract: Option<String>,
    /// Amount of each swap token per sender (in wei, as string).
    pub swap_token_amount: String,
}
