#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod block;
pub use block::meter_block;

mod estimator;
pub use estimator::{
    EstimateError, MeteredTransaction, ResourceDemand, ResourceEstimate, ResourceEstimates,
    ResourceKind, ResourceLimits, compute_estimate, estimate_from_transactions, usage_extractor,
};

mod extension;
pub use extension::{MeteringExtension, MeteringExtensionConfig, MeteringResourceLimits};

mod meter;
pub use meter::{MeterBundleOutput, meter_bundle};

mod rpc;
pub use rpc::MeteringApiImpl;

mod traits;
pub use traits::MeteringApiServer;

mod types;
pub use types::{
    MeterBlockResponse, MeterBlockTransactions, MeteredPriorityFeeResponse,
    ResourceFeeEstimateResponse,
};
