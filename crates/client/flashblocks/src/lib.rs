#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[macro_use]
extern crate tracing;

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

mod validation;
pub use validation::{
    CanonicalBlockReconciler, FlashblockSequenceValidator, ReconciliationStrategy,
    ReorgDetectionResult, ReorgDetector, SequenceValidationResult,
};

mod rpc;
pub use rpc::{
    BaseSubscriptionKind, EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer,
    ExtendedSubscriptionKind,
};

mod extension;
pub use extension::{FlashblocksConfig, FlashblocksExtension};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_harness;
