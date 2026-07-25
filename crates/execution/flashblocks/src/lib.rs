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
    AuthorityRecordHasherV1, CliRegistryLookupFailed, CliRegistryLookupFailureReason,
    ClockAnchorRecordV1, ClockFailureV1, ClockIdV1, ClockStatusV1, DecodedFlashblockKeyV1,
    EDGE_ACTIVE_STATE_CAPACITY_MAX_V1, EDGE_ANCHOR_CADENCE_NS_V1, EDGE_CLOCK_SOURCE_VERSION_V1,
    EDGE_EVENT_QUEUE_CAPACITY_MAX_V1, EdgeEventDrainStatusV1, EdgeMeasurementGlobal,
    EdgeMeasurementInstallConfigV1, EdgeMeasurementInstallErrorV1, EdgeMeasurementPoisonV1,
    EdgeMeasurementRecorderHandleV1, EdgeMeasurementRecorderStateV1, EdgeMeasurementRecorderV1,
    EdgeMeasurementRegistryHandleV2, EdgeSourceEventV1, EdgeSourceFinalSealErrorV1,
    EpochAdmissionTokenV1, EpochRouteV1, PENDING_REGISTRY_CAPACITY_V2, PayloadFirstKeyV1,
    PayloadFirstObservationV1, PendingAccountingFieldV2, PendingCleanupEventV2,
    PendingCliTerminalV2, PendingFinalSealErrorV2, PendingMetadataRegistryV2,
    PendingPublicSubsetHasherV1, PendingRegistrationAttemptV2, PendingRegistrationDispositionV2,
    PendingRegistrationFailure, PendingRegistryCountersV2, PendingRegistryEntryV2,
    PendingRegistryError, PendingRegistryPoisonV2, PendingRegistrySequenceSetsV2,
    PendingRegistrySnapshotV2, PendingRegistryStateV2, PendingSendDispositionV2,
    PendingSendJournalEntryV2, PendingSendJournalMarkerV2, PendingSequenceBitmapV2,
    PendingSnapshotIdentityV2, PendingSnapshotMetadataV2, PendingTerminalRecordV2,
    ProcessorBaseDispositionV1, ProcessorLifecycleProductV1, ProcessorObserverDispositionV1,
    ProcessorPublishDispositionV1, ProducerEpochCutoffV1, ProducerExternalBoundsV1,
    SourceConnectionErrorClassV1, SourceConnectionRecordV1, SourceConnectionTransitionV1,
    SourceCoverageRecordV3, SourceCoverageTerminalV3, SourceTerminalCoverageV3,
    WireLifecycleTransitionV1, WireObservationV1,
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
