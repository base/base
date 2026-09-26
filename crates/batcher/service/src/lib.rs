#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::BatcherConfig;

mod metrics;
pub use metrics::L2BlockParityMetrics;

mod recent_txs;
pub use recent_txs::{MAX_CHECK_RECENT_TXS_DEPTH, RecentTxSyncTarget};

mod system_config;
pub use system_config::{ISystemConfig, SystemConfigBatcher};

mod source;
pub use source::RpcPollingSource;

mod l1_source;
pub use l1_source::RpcL1HeadPollingSource;

mod l2_block_parity;
pub use l2_block_parity::{
    DEFAULT_MAX_BLOCKS_PER_TICK, L2BlockParityMonitor, L2BlockParityMonitorConfig,
    L2BlockParityResult, L2BlockParityStats, L2BlockProvider, L2BlockSnapshot, RpcL2BlockProvider,
};

mod throttle;
pub use throttle::RpcThrottleClient;

mod derivation_status_poller;
pub use derivation_status_poller::{DerivationStatusPoller, DerivationStatusProvider};

mod service;
pub use service::{BatcherService, ReadyBatcher};
