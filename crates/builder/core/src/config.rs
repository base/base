//! Builder configuration.

use core::time::Duration;

use base_execution_payload_builder::config::{BaseDAConfig, GasLimitConfig};

use crate::SharedMeteringProvider;

/// Configuration values for the Base builder binary.
#[derive(Clone)]
pub struct BuilderConfig {
    /// The interval at which blocks are added to the chain.
    /// This is also the frequency at which the builder will be receiving FCU requests from the
    /// sequencer.
    pub block_time: Duration,

    /// Data Availability configuration for the payload builder.
    /// Defines constraints for the maximum size of data availability transactions.
    pub da_config: BaseDAConfig,

    /// Gas limit configuration for the payload builder
    pub gas_limit_config: GasLimitConfig,

    /// Extra time allowed for payload building before garbage collection.
    pub block_time_leeway: Duration,

    /// Inverted sampling frequency in blocks. 1 - each block, 100 - every 100th block.
    pub sampling_ratio: u64,

    /// Maximum gas a transaction can use before being excluded.
    pub max_gas_per_txn: Option<u64>,

    /// Maximum execution time per transaction in microseconds.
    pub max_execution_time_per_tx_us: Option<u128>,

    /// Whole-block execution time budget in microseconds.
    pub block_execution_time_budget_us: Option<u128>,

    /// Block-level state root gas limit.
    ///
    /// State root gas is a synthetic resource that accumulates like gas but penalizes
    /// transactions whose simulated state root cost is disproportionate to their gas usage.
    /// For each metered transaction: `sr_gas = gas_used × (1 + K × max(0, SR_ms - anchor))`.
    /// Normal transactions (SR ≤ anchor) pay 1:1. State-heavy transactions pay more.
    pub block_state_root_gas_limit: Option<u64>,

    /// State root gas coefficient (K). Controls how aggressively excess SR time
    /// inflates the state root gas cost. Default: 0.02.
    pub state_root_gas_coefficient: f64,

    /// State root gas anchor in microseconds. SR time below this threshold
    /// produces no penalty (multiplier = 1). Default: 5000 (5ms).
    pub state_root_gas_anchor_us: u128,

    /// Maximum cumulative uncompressed (EIP-2718 encoded) block size in bytes.
    pub max_uncompressed_block_size: Option<u64>,

    /// Duration to wait for metering data before including a transaction.
    /// Transactions younger than this without metering data will be skipped.
    pub metering_wait_duration: Option<Duration>,

    /// Resource metering provider
    pub metering_provider: SharedMeteringProvider,

    /// URL of the audit-archiver RPC endpoint for rejected transaction forwarding.
    /// When set, rejected transactions will be forwarded to this endpoint.
    pub audit_archiver_url: Option<String>,

    /// Bounded channel capacity for rejected transaction forwarding.
    /// When the channel is full, new rejected transactions are dropped.
    pub rejected_tx_channel_size: usize,

    /// Maximum number of rejected transactions accumulated per block before
    /// further rejections are dropped. Prevents unbounded `ExecutionInfo` growth.
    pub max_rejected_txs_per_block: usize,
}

impl core::fmt::Debug for BuilderConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Config")
            .field("block_time", &self.block_time)
            .field("block_time_leeway", &self.block_time_leeway)
            .field("da_config", &self.da_config)
            .field("gas_limit_config", &self.gas_limit_config)
            .field("sampling_ratio", &self.sampling_ratio)
            .field("max_gas_per_txn", &self.max_gas_per_txn)
            .field("max_execution_time_per_tx_us", &self.max_execution_time_per_tx_us)
            .field("block_execution_time_budget_us", &self.block_execution_time_budget_us)
            .field("block_state_root_gas_limit", &self.block_state_root_gas_limit)
            .field("state_root_gas_coefficient", &self.state_root_gas_coefficient)
            .field("state_root_gas_anchor_us", &self.state_root_gas_anchor_us)
            .field("max_uncompressed_block_size", &self.max_uncompressed_block_size)
            .field("metering_wait_duration", &self.metering_wait_duration)
            .field("metering_provider", &self.metering_provider)
            .field("audit_archiver_url", &self.audit_archiver_url)
            .field("rejected_tx_channel_size", &self.rejected_tx_channel_size)
            .field("max_rejected_txs_per_block", &self.max_rejected_txs_per_block)
            .finish()
    }
}
