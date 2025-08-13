use metrics::IntoF64;
use reth_metrics::{
    metrics::{gauge, Counter, Gauge, Histogram},
    Metrics,
};

use crate::args::OpRbuilderArgs;

/// The latest version from Cargo.toml.
pub const CARGO_PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The 8 character short SHA of the latest commit.
pub const VERGEN_GIT_SHA: &str = env!("VERGEN_GIT_SHA_SHORT");

/// The build timestamp.
pub const VERGEN_BUILD_TIMESTAMP: &str = env!("VERGEN_BUILD_TIMESTAMP");

/// The target triple.
pub const VERGEN_CARGO_TARGET_TRIPLE: &str = env!("VERGEN_CARGO_TARGET_TRIPLE");

/// The build features.
pub const VERGEN_CARGO_FEATURES: &str = env!("VERGEN_CARGO_FEATURES");

/// The build profile name.
pub const BUILD_PROFILE_NAME: &str = env!("OP_RBUILDER_BUILD_PROFILE");

pub const VERSION: VersionInfo = VersionInfo {
    version: CARGO_PKG_VERSION,
    build_timestamp: VERGEN_BUILD_TIMESTAMP,
    cargo_features: VERGEN_CARGO_FEATURES,
    git_sha: VERGEN_GIT_SHA,
    target_triple: VERGEN_CARGO_TARGET_TRIPLE,
    build_profile: BUILD_PROFILE_NAME,
};

/// op-rbuilder metrics
#[derive(Metrics, Clone)]
#[metrics(scope = "op_rbuilder")]
pub struct OpRBuilderMetrics {
    /// Block built success
    pub block_built_success: Counter,
    /// Number of flashblocks added to block (Total per block)
    pub flashblock_count: Histogram,
    /// Number of messages sent
    pub messages_sent_count: Counter,
    /// Histogram of the time taken to build a block
    pub total_block_built_duration: Histogram,
    /// Latest time taken to build a block
    pub total_block_built_gauge: Gauge,
    /// Histogram of the time taken to build a Flashblock
    pub flashblock_build_duration: Histogram,
    /// Flashblock UTF8 payload byte size histogram
    pub flashblock_byte_size_histogram: Histogram,
    /// Histogram of transactions in a Flashblock
    pub flashblock_num_tx_histogram: Histogram,
    /// Number of invalid blocks
    pub invalid_blocks_count: Counter,
    /// Histogram of fetching transactions from the pool duration
    pub transaction_pool_fetch_duration: Histogram,
    /// Latest time taken to fetch tx from the pool
    pub transaction_pool_fetch_gauge: Gauge,
    /// Histogram of state root calculation duration
    pub state_root_calculation_duration: Histogram,
    /// Latest state root calculation duration
    pub state_root_calculation_gauge: Gauge,
    /// Histogram of sequencer transaction execution duration
    pub sequencer_tx_duration: Histogram,
    /// Latest sequencer transaction execution duration
    pub sequencer_tx_gauge: Gauge,
    /// Histogram of state merge transitions duration
    pub state_transition_merge_duration: Histogram,
    /// Latest state merge transitions duration
    pub state_transition_merge_gauge: Gauge,
    /// Histogram of the duration of payload simulation of all transactions
    pub payload_tx_simulation_duration: Histogram,
    /// Latest payload simulation of all transactions duration
    pub payload_tx_simulation_gauge: Gauge,
    /// Number of transaction considered for inclusion in the block
    pub payload_num_tx_considered: Histogram,
    /// Latest number of transactions considered for inclusion in the block
    pub payload_num_tx_considered_gauge: Gauge,
    /// Payload byte size histogram
    pub payload_byte_size: Histogram,
    /// Latest Payload byte size
    pub payload_byte_size_gauge: Gauge,
    /// Histogram of transactions in the payload
    pub payload_num_tx: Histogram,
    /// Latest number of transactions in the payload
    pub payload_num_tx_gauge: Gauge,
    /// Histogram of transactions in the payload that were successfully simulated
    pub payload_num_tx_simulated: Histogram,
    /// Latest number of transactions in the payload that were successfully simulated
    pub payload_num_tx_simulated_gauge: Gauge,
    /// Histogram of transactions in the payload that were successfully simulated
    pub payload_num_tx_simulated_success: Histogram,
    /// Latest number of transactions in the payload that were successfully simulated
    pub payload_num_tx_simulated_success_gauge: Gauge,
    /// Histogram of transactions in the payload that failed simulation
    pub payload_num_tx_simulated_fail: Histogram,
    /// Latest number of transactions in the payload that failed simulation
    pub payload_num_tx_simulated_fail_gauge: Gauge,
    /// Histogram of tx simulation duration
    pub tx_simulation_duration: Histogram,
    /// Byte size of transactions
    pub tx_byte_size: Histogram,
    /// Da block size limit
    pub da_block_size_limit: Gauge,
    /// Da tx size limit
    pub da_tx_size_limit: Gauge,
    /// How much less flashblocks we issue to be on time with block construction
    pub reduced_flashblocks_number: Histogram,
    /// How much less flashblocks we issued in reality, comparing to calculated number for block
    pub missing_flashblocks_count: Histogram,
    /// How much time we have deducted from block building time
    pub flashblocks_time_drift: Histogram,
    /// Time offset we used for first flashblock
    pub first_flashblock_time_offset: Histogram,
    /// Number of requests sent to the eth_sendBundle endpoint
    pub bundle_requests: Counter,
    /// Number of valid bundles received at the eth_sendBundle endpoint
    pub valid_bundles: Counter,
    /// Number of bundles that failed to execute
    pub failed_bundles: Counter,
    /// Number of reverted bundles
    pub bundles_reverted: Histogram,
    /// Histogram of eth_sendBundle request duration
    pub bundle_receive_duration: Histogram,
}

impl OpRBuilderMetrics {
    pub fn set_payload_builder_metrics(
        &self,
        payload_tx_simulation_time: impl IntoF64 + Copy,
        num_txs_considered: impl IntoF64 + Copy,
        num_txs_simulated: impl IntoF64 + Copy,
        num_txs_simulated_success: impl IntoF64 + Copy,
        num_txs_simulated_fail: impl IntoF64 + Copy,
        num_bundles_reverted: impl IntoF64,
    ) {
        self.payload_tx_simulation_duration
            .record(payload_tx_simulation_time);
        self.payload_tx_simulation_gauge
            .set(payload_tx_simulation_time);
        self.payload_num_tx_considered.record(num_txs_considered);
        self.payload_num_tx_considered_gauge.set(num_txs_considered);
        self.payload_num_tx_simulated.record(num_txs_simulated);
        self.payload_num_tx_simulated_gauge.set(num_txs_simulated);
        self.payload_num_tx_simulated_success
            .record(num_txs_simulated_success);
        self.payload_num_tx_simulated_success_gauge
            .set(num_txs_simulated_success);
        self.payload_num_tx_simulated_fail
            .record(num_txs_simulated_fail);
        self.payload_num_tx_simulated_fail_gauge
            .set(num_txs_simulated_fail);
        self.bundles_reverted.record(num_bundles_reverted);
    }
}

/// Set gauge metrics for some flags so we can inspect which ones are set
/// and which ones aren't.
pub fn record_flag_gauge_metrics(builder_args: &OpRbuilderArgs) {
    gauge!("op_rbuilder_flags_flashblocks_enabled").set(builder_args.flashblocks.enabled as i32);
    gauge!("op_rbuilder_flags_flashtestations_enabled")
        .set(builder_args.flashtestations.flashtestations_enabled as i32);
    gauge!("op_rbuilder_flags_enable_revert_protection")
        .set(builder_args.enable_revert_protection as i32);
}

/// Contains version information for the application.
#[derive(Debug, Clone)]
pub struct VersionInfo {
    /// The version of the application.
    pub version: &'static str,
    /// The build timestamp of the application.
    pub build_timestamp: &'static str,
    /// The cargo features enabled for the build.
    pub cargo_features: &'static str,
    /// The Git SHA of the build.
    pub git_sha: &'static str,
    /// The target triple for the build.
    pub target_triple: &'static str,
    /// The build profile (e.g., debug or release).
    pub build_profile: &'static str,
}

impl VersionInfo {
    /// This exposes reth's version information over prometheus.
    pub fn register_version_metrics(&self) {
        let labels: [(&str, &str); 6] = [
            ("version", self.version),
            ("build_timestamp", self.build_timestamp),
            ("cargo_features", self.cargo_features),
            ("git_sha", self.git_sha),
            ("target_triple", self.target_triple),
            ("build_profile", self.build_profile),
        ];

        let gauge = gauge!("builder_info", &labels);
        gauge.set(1);
    }
}
