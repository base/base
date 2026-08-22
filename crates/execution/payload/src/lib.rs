#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

pub mod builder;
pub use builder::BasePayloadBuilder;
pub mod config;
pub use config::ResourceMeteringConfig;
pub mod error;
mod metering;
pub use metering::{MeteringProvider, NoopMeteringProvider, SharedMeteringProvider};
mod metrics;
pub use metrics::ResourceMeteringMetrics;
mod resource_metering;
pub use resource_metering::{
    CompiledResourceMeteringDimension, CompiledResourceMeteringSchedule, ResourceMeteringDimension,
    ResourceMeteringError, ResourceMeteringOperation, ResourceMeteringSchedule,
    ResourceMeteringUsage, ResourceSample, ResourceThrottlingCheckError,
    ResourceThrottlingDecision, ResourceThrottlingLimitExceeded, ResourceThrottlingLimitScope,
};
pub mod payload;
pub use payload::{BaseBuiltPayload, BasePayloadBuilderAttributes};
mod traits;
pub use traits::*;
mod types;
pub use types::BasePayloadTypes;
pub mod validator;
pub use validator::BaseExecutionPayloadValidator;
