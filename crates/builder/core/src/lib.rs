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
pub use metrics::{BUILDER_EVENT_METRICS, BuilderEventMetrics, BuilderMetrics, CounterHandle};

mod execution;
pub use execution::{
    ExecutionInfo, ExecutionMeteringLimitExceeded, FlashblocksExecutionInfo, ResourceLimits,
    TxResources, TxnExecutionError, TxnOutcome,
};

mod execution_metering_mode;
pub use execution_metering_mode::ExecutionMeteringMode;

mod resting_predicate_mode;
pub use resting_predicate_mode::RestingPredicateMode;

mod traits;
pub use base_execution_payload_builder::{
    MeteringProvider, NoopMeteringProvider, RejectionCache, ResourceMeteringConfig,
    SharedMeteringProvider,
};
pub use traits::{ClientBounds, NodeBounds, PayloadTxsBounds, PoolBounds};

// Internal-only helpers for emitting builder transaction events. The event surface
// is shared via `base-observability-events`, while this module keeps
// builder-specific payload construction private to the builder crate.
mod transaction_events;

mod flashblocks;
pub use flashblocks::{
    BasePayloadBuilderCtx, BestFlashblocksTxs, BlockDeferrals, BlockPayloadJob,
    BlockPayloadJobGenerator, BuildArguments, FLOW_STANDARD, FLOW_VALIDITY, FlashblockDiagnostics,
    FlashblockSelectionOutcome, FlashblocksExtraCtx, FlashblocksServiceBuilder, InclusionFlow,
    InclusionTracker, ParkableBestPayloadTransactions, ParkablePayloadTransactions,
    ParkedPredicateIndex, PayloadBuilder, PayloadHandler, PayloadJobDeadline,
    PayloadTransactionInvalidated, PredicateLoadTracker, PredicateReadRecorder, ResolvePayload,
    RestingPayloadTransactions, RestingStats, StateChangeEffects, ValidityPredicateEvaluation,
    ValidityPredicateKey,
};

mod extension;
pub use extension::{
    BuilderApiExtension, BuilderApiExtensionArgs, BuilderApiExtensionConfig,
    DEFAULT_MAX_VALIDITY_PREDICATES, ValiditySignatureMode,
};

mod shadow_validity;
pub use shadow_validity::{
    MAX_SHADOW_VALIDITY_SAMPLE_RATE_BPS, ShadowValidityBuilderApi, ShadowValidityConfig,
    ShadowValidityConfigError,
};

/// Shared test infrastructure: local node instances, chain drivers, transaction builders, and pool observers.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
