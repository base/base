#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), allow(unused_crate_dependencies))]

mod config;
pub use config::BuilderConfig;

mod metrics;
pub use metrics::BuilderMetrics;

mod execution;
pub use execution::{
    ExecutionInfo, ExecutionMeteringLimitExceeded, ResourceLimits, TxResources, TxnExecutionError,
    TxnOutcome,
};

mod execution_metering_mode;
pub use execution_metering_mode::ExecutionMeteringMode;

mod traits;
pub use traits::{ClientBounds, NodeBounds, PayloadTxsBounds, PoolBounds};

mod metering;
pub use metering::{MeteringProvider, NoopMeteringProvider, SharedMeteringProvider};

mod flashblocks;
pub use flashblocks::{
    BasePayloadBuilderCtx, BestFlashblocksTxs, BlockCell, BlockPayloadJob,
    BlockPayloadJobGenerator, BuildArguments, FlashblockDiagnostics, FlashblockSelectionOutcome,
    FlashblocksExecutionInfo, FlashblocksExtraCtx, FlashblocksServiceBuilder, PayloadBuilder,
    PayloadHandler, RejectionCache, ResolvePayload, WaitForValue,
};

mod extension;
pub use extension::BuilderApiExtension;

/// Shared test infrastructure: local node instances, chain drivers, transaction builders, and pool observers.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
