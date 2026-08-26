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

mod dowse;
pub use dowse::{DowseConfig, DowsePrefetchCache, DowsePrefetchWork, DowsePrefetcher};

mod execution;
pub use execution::{
    ExecutionInfo, ExecutionMeteringLimitExceeded, FlashblocksExecutionInfo, ResourceLimits,
    TxResources, TxnExecutionError, TxnOutcome,
};

mod execution_metering_mode;
pub use execution_metering_mode::ExecutionMeteringMode;

mod traits;
pub use traits::{ClientBounds, NodeBounds, PayloadTxsBounds, PoolBounds};

mod metering;
pub use metering::{MeteringProvider, NoopMeteringProvider, SharedMeteringProvider};

mod rejected_tx_forwarder;
pub use rejected_tx_forwarder::RejectedTxForwarder;

// Internal-only helpers for emitting builder transaction events. The event surface
// is shared via `base-observability-events`, while this module keeps
// builder-specific payload construction private to the builder crate.
mod transaction_events;

mod rejection_cache;
pub use rejection_cache::RejectionCache;

mod flashblocks;
pub use flashblocks::{
    BasePayloadBuilderCtx, BestFlashblocksTxs, BlockPayloadJob, BlockPayloadJobGenerator,
    BuildArguments, FLOW_STANDARD, FLOW_VALIDITY, FlashblockDiagnostics,
    FlashblockSelectionOutcome, FlashblocksExtraCtx, FlashblocksServiceBuilder, InclusionFlow,
    InclusionTracker, ParkableBestPayloadTransactions, ParkablePayloadTransactions,
    ParkedPredicateIndex, PayloadBuilder, PayloadHandler, PayloadJobDeadline,
    PayloadTransactionInvalidated, PredicateLoadTracker, PredicateReadRecorder, ResolvePayload,
    StateChangeEffects, ValidityPredicateKey,
};

mod extension;
pub use extension::{
    BuilderApiExtension, BuilderApiExtensionConfig, DEFAULT_MAX_VALIDITY_PREDICATES,
};

mod shadow_validity;
pub use shadow_validity::{
    MAX_SHADOW_VALIDITY_SAMPLE_RATE_BPS, ShadowValidityBuilderApi, ShadowValidityConfig,
    ShadowValidityConfigError,
};

/// Shared test infrastructure: local node instances, chain drivers, transaction builders, and pool observers.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
