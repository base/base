#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

mod affordability;
pub use affordability::CoinbaseTipAffordability;
pub mod builder;
pub use builder::BasePayloadBuilder;
pub mod config;
pub use config::ResourceMeteringConfig;
mod rejection_cache;
pub use rejection_cache::{REJECTION_CACHE_MAX_CAPACITY, REJECTION_CACHE_TTL, RejectionCache};
pub mod error;
mod metering;
pub use metering::{MeteringProvider, NoopMeteringProvider, SharedMeteringProvider};
mod resource_metering;
pub use resource_metering::{
    ResourceMeteringDimension, ResourceMeteringError, ResourceMeteringOperation,
    ResourceMeteringSchedule, ResourceMeteringUsage, ResourceSample, ResourceThrottlingCheckError,
    ResourceThrottlingDecision, ResourceThrottlingLimitExceeded, ResourceThrottlingLimitScope,
};
mod resource_metering_metrics;
pub use resource_metering_metrics::{RejectionCacheMetrics, ResourceMeteringMetrics};
pub mod payload;
pub use payload::{BaseBuiltPayload, BasePayloadBuilderAttributes};

mod parkable;
pub use parkable::{
    NonParkablePayloadTransactions, NoopPayloadTransactions, ParkableBestPayloadTransactions,
    ParkablePayloadTransactions, PayloadTransactionInvalidated,
};

mod metrics;
pub use metrics::{BuilderMetrics, ValidityMetrics};

mod inclusion;
pub use inclusion::{FLOW_STANDARD, FLOW_VALIDITY, InclusionFlow, InclusionTracker};

mod predicate_loads;
pub use predicate_loads::{PredicateLoadTracker, PredicateReadRecorder};

mod traits;
pub use traits::*;
mod types;
pub use types::BasePayloadTypes;

mod validity;
pub use validity::{
    ParkedPredicateIndex, StateChangeEffects, ValidityPredicateEvaluation, ValidityPredicateKey,
};

pub mod validator;
pub use validator::BaseExecutionPayloadValidator;
