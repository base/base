#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[macro_use]
extern crate tracing;

mod block_assembler;
pub use block_assembler::{AssembledBlock, BlockAssembler};

mod cache;
pub use cache::FlashblockCache;

mod error;
pub use error::{
    BuildError, ExecutionError, ProtocolError, ProviderError, Result, StateProcessorError,
};

#[cfg(feature = "edge-measurement")]
mod edge_measurement;
#[cfg(feature = "edge-measurement")]
pub use edge_measurement::{
    CliRegistryLookupFailed, CliRegistryLookupFailureReason, DecodedFlashblockKeyV1,
    EdgeMeasurementGlobal, EdgeMeasurementRecorderStateV1, EdgeMeasurementRecorderV1,
    PENDING_REGISTRY_CAPACITY_V2, PayloadFirstKeyV1, PayloadFirstObservationV1,
    PendingCliTerminalV2, PendingMetadataRegistryV2, PendingPublicSubsetHasherV1,
    PendingRegistrationAttemptV2, PendingRegistrationDispositionV2, PendingRegistrationFailure,
    PendingRegistryCountersV2, PendingRegistryEntryV2, PendingRegistryError,
    PendingRegistrySnapshotV2, PendingRegistryStateV2, PendingSendDispositionV2,
    PendingSnapshotIdentityV2, PendingSnapshotMetadataV2, PendingTerminalRecordV2,
    ProducerEpochCutoffV1, SourceConnectionRecordV1, SourceConnectionTransitionV1,
    WireObservationV1,
};

mod metrics;
pub use metrics::Metrics;

mod observer;
pub use observer::PendingFrameObserver;
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
    BaseSubscriptionKind, BlockNumberOrTagExt, EthApiExt, EthApiOverrideServer, EthPubSub,
    EthPubSubApiServer, ExtendedSubscriptionKind, TransactionWithLogs,
};
