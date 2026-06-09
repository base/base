#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod block;
pub use block::meter_block;

mod cache;
pub use cache::{BlockMetrics, MeteredTransaction, MeteringCache, ResourceTotals, SegmentMetrics};

mod estimator;
pub use estimator::{
    BlockPriorityEstimates, EstimateError, PriorityFeeEstimator, ResourceDemand, ResourceEstimate,
    ResourceEstimates, ResourceKind, ResourceLimits, RollingPriorityEstimate,
    SegmentResourceEstimates,
};

mod extension;
pub use extension::{MeteringConfig, MeteringExtension, MeteringResourceLimits};

mod inspector;

mod meter;
pub use meter::{MeterBundleInput, MeterBundleOutput, MeteredOpcodes, meter_bundle};

mod metrics;

mod rpc;
pub use rpc::MeteringApiImpl;

mod traits;
pub use traits::MeteringApiServer;

mod types;
pub use types::{
    MeterBlockResponse, MeterBlockTransactions, MeteredPriorityFeeResponse,
    ResourceFeeEstimateResponse,
};

mod transaction;
pub use transaction::{TxValidationError, validate_tx};
