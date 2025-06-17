use reth_metrics::{
    metrics::{gauge, Counter, Gauge, Histogram},
    Metrics,
};

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
    /// Total duration of building a block
    pub total_block_built_duration: Histogram,
    /// Flashblock build duration
    pub flashblock_build_duration: Histogram,
    /// Number of invalid blocks
    pub invalid_blocks_count: Counter,
    /// Duration of fetching transactions from the pool
    pub transaction_pool_fetch_duration: Histogram,
    /// Duration of state root calculation
    pub state_root_calculation_duration: Histogram,
    /// Duration of sequencer transaction execution
    pub sequencer_tx_duration: Histogram,
    /// Duration of state merge transitions
    pub state_transition_merge_duration: Histogram,
    /// Duration of payload simulation of all transactions
    pub payload_tx_simulation_duration: Histogram,
    /// Number of transaction considered for inclusion in the block
    pub payload_num_tx_considered: Histogram,
    /// Payload byte size
    pub payload_byte_size: Histogram,
    /// Number of transactions in the payload
    pub payload_num_tx: Histogram,
    /// Number of transactions in the payload that were successfully simulated
    pub payload_num_tx_simulated: Histogram,
    /// Number of transactions in the payload that were successfully simulated
    pub payload_num_tx_simulated_success: Histogram,
    /// Number of transactions in the payload that failed simulation
    pub payload_num_tx_simulated_fail: Histogram,
    /// Duration of tx simulation
    pub tx_simulation_duration: Histogram,
    /// Byte size of transactions
    pub tx_byte_size: Histogram,
    /// Da block size limit
    pub da_block_size_limit: Gauge,
    /// Da tx size limit
    pub da_tx_size_limit: Gauge,
    /// Desired number of flashblocks
    pub target_flashblock: Histogram,
    /// Time drift that we account for in the beginning of block building
    pub flashblock_time_drift: Histogram,
    /// Time offset we used for first flashblock
    pub first_flashblock_time_offset: Histogram,
    /// Number of valid bundles received at the eth_sendBundle endpoint
    pub bundles_received: Counter,
    /// Number of reverted bundles
    pub bundles_reverted: Histogram,
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
