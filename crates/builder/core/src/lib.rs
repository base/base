#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), allow(unused_crate_dependencies))]

mod metrics;
pub use base_execution_payload_builder::{
    MeteringProvider, NoopMeteringProvider, RejectionCache, ResourceMeteringConfig,
    RestingPredicateMode, SharedMeteringProvider, config::BaseBuilderConfig,
};
pub use metrics::BuilderMetrics;

mod service;
pub use service::BuilderServiceBuilder;

mod extension;
pub use extension::{
    BuilderApiExtension, BuilderApiExtensionArgs, BuilderApiExtensionConfig,
    DEFAULT_MAX_VALIDITY_PREDICATES, InsertMeteringAdapter, ValiditySignatureMode,
};

/// Shared test infrastructure: local node instances, chain drivers, transaction builders, and pool observers.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
