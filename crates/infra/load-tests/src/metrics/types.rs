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
    /// Time from submission to sequencer acceptance.
    pub flashblocks_latency: Option<Duration>,
    /// Gas used by the transaction.
    pub gas_used: u64,
    /// Gas price in wei.
    pub gas_price: u128,
    /// Block number where transaction was included.
    pub block_number: Option<u64>,
    /// When the confirmer discovered the receipt (used by the rolling window).
    #[serde(skip)]
    pub confirmed_at: Option<Instant>,
}

impl TransactionMetrics {
    /// Creates new transaction metrics.
    pub fn new(
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
