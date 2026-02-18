#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), allow(unused_crate_dependencies))]

// Re-export types from the payload builder crate.
pub use base_payload_builder::{
    BestFlashblocksTxs, BlockAssembler, BlockCell, BlockRoots, BuildArguments, BuilderMetrics,
    ClientBounds, ExecutionInfo, ExecutionMeteringLimitExceeded, ExecutionMeteringMode,
    FlashblockBatchLimits, FlashblockLimits, FlashblockPublisher, FlashblockScheduler,
    FlashblocksConfig, FlashblocksExecutionInfo, FlashblocksExtraCtx, GuardedPublisher,
    MeteringProvider, NoopFlashblockPublisher, NoopMeteringProvider, OpPayloadBuilder,
    OpPayloadBuilderCtx, PayloadBuilder, PayloadBuilderConfig, PayloadTxsBounds, PoolBounds,
    PublishResult, ResourceLimits, SharedMeteringProvider, TxExecutor, TxResources,
    TxnExecutionError, TxnOutcome, WaitForValue,
};

mod config;
pub use config::BuilderConfig;

mod traits;
pub use traits::NodeBounds;

mod flashblocks;
pub use flashblocks::{
    BlockPayloadJob, BlockPayloadJobGenerator, FlashblocksServiceBuilder, PayloadHandler,
    ResolvePayload,
};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
