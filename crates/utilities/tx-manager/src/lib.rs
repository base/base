#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
pub use error::{
    REVERT_DATA_DISPLAY_LIMIT, RevertDisplay, RpcErrorClassifier, TxManagerError, TxManagerResult,
};

mod candidate;
pub use candidate::TxCandidate;

mod fees;
pub use fees::{BumpedFees, FeeCalculator, FeeOverride, GasPriceCaps};

mod macros;

mod config;
pub use config::{ConfigError, GweiParser, TxManagerConfig};

mod signer_config;
pub use signer_config::SignerConfig;

mod submission;
pub use submission::{
    SubmissionCompletion, SubmissionHandle, SubmissionId, SubmissionResult, SubmissionSnapshot,
    SubmissionStatus, SubmissionTracker,
};

mod manager;
pub use manager::{
    AcceptedPosition, AttemptedPosition, CancelRequest, ChainSweeper, CoordinatorCommand,
    CoordinatorHandle, CoordinatorWorkers, MAX_CONCURRENT_SWEEP_QUERIES, NonceFetch, NonceSlot,
    PendingLedger, PendingPolicy, PendingWork, PreparedTx, PublishOutcome, PublishReject,
    PublishedAttempt, PublisherCursor, PublisherEvent, PublisherGroup, PublisherId,
    PublisherSnapshot, PublisherTx, RejectionVerdict, ReplacementReason, ReplacementState,
    SUPERSESSION_OBSERVATIONS, SignedVersion, SimpleTxManager, SlotEffects, SlotState,
    StagedSubmission, SupersessionEvidence, SweepOutcome, SweepResolution, SweepTarget, TxBuilder,
    TxCoordinator, TxManager, TxPublisher, VersionId, VersionKind, WEI_PER_GWEI, WorkerEvent,
};

mod metrics;
pub use metrics::{BaseTxMetrics, NoopTxMetrics, TxManagerMetrics, TxMetrics};

mod blob;
pub use blob::{BlobTxBuilder, MAX_BLOBS_PER_TX};

#[cfg(test)]
pub mod test_utils;
#[cfg(test)]
pub use test_utils::StubReceipt;
