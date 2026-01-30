#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[macro_use]
extern crate tracing;

mod block_assembler;
pub use block_assembler::{AssembledBlock, BlockAssembler};

mod error;
pub use error::{
    BuildError, ExecutionError, ProtocolError, ProviderError, Result, StateProcessorError,
};

mod metrics;
pub use metrics::Metrics;

mod pending_blocks;
pub use pending_blocks::{PendingBlocks, PendingBlocksBuilder};

mod processor;
pub use processor::{StateProcessor, StateUpdate};

mod state;
pub use state::FlashblocksState;

mod subscription;
pub use subscription::FlashblocksSubscriber;

mod traits;
pub use traits::{FlashblocksAPI, FlashblocksReceiver, PendingBlocksAPI};

mod state_builder;
pub use state_builder::{ExecutedPendingTransaction, PendingStateBuilder};

mod receipt_builder;
pub use receipt_builder::{ReceiptBuildError, UnifiedReceiptBuilder};

mod validation;
pub use validation::{
    CanonicalBlockReconciler, FlashblockSequenceValidator, ReconciliationStrategy,
    ReorgDetectionResult, ReorgDetector, SequenceValidationResult,
};

mod config;
pub use config::FlashblocksConfig;

mod rpc;
pub use rpc::{
    BaseSubscriptionKind, EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer,
    ExtendedSubscriptionKind, TransactionWithLogs,
};
