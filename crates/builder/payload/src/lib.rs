#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod execution_metering_mode;
pub use execution_metering_mode::ExecutionMeteringMode;

mod metering;
pub use metering::{MeteringProvider, NoopMeteringProvider, SharedMeteringProvider};

mod info;
pub use info::FlashblocksExecutionInfo;

mod execution;
pub use execution::{
    ExecutionInfo, ExecutionMeteringLimitExceeded, ResourceLimits, TxResources, TxnExecutionError,
    TxnOutcome,
};

mod config;
pub use config::{FlashblocksConfig, PayloadBuilderConfig};

mod publisher;
pub use publisher::{FlashblockPublisher, GuardedPublisher, NoopFlashblockPublisher, PublishResult};

mod scheduler;
pub use scheduler::FlashblockScheduler;

mod limits;
pub use limits::{FlashblockBatchLimits, FlashblockLimits};

mod metrics;
pub use metrics::BuilderMetrics;

mod bounds;
pub use bounds::{ClientBounds, PayloadTxsBounds, PoolBounds};

mod cell;
pub use cell::{BlockCell, WaitForValue};

/// Best transactions adapter for flashblock building.
pub mod best_txs;
pub use best_txs::BestFlashblocksTxs;

mod args;
pub use args::BuildArguments;

mod context;
pub use context::{FlashblocksExtraCtx, OpPayloadBuilderCtx};

mod executor;
pub use executor::TxExecutor;

mod assembler;
pub use assembler::{BlockAssembler, BlockRoots};

mod builder;
pub use builder::{OpPayloadBuilder, PayloadBuilder};
