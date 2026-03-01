#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod block;
pub use block::meter_block;

mod estimator;
pub use estimator::{
    EstimateError, MeteredTransaction, ResourceDemand, ResourceEstimate, ResourceEstimates,
    ResourceKind, ResourceLimits,
};

mod extension;
pub use extension::{MeteringConfig, MeteringExtension, MeteringResourceLimits};

mod meter;
pub use meter::{MeterBundleOutput, PendingState, PendingTrieInput, meter_bundle};

mod metrics;

mod rpc;
pub use rpc::MeteringApiImpl;

mod traits;
pub use traits::MeteringApiServer;

mod trie_cache;
pub use trie_cache::PendingTrieCache;

mod types;
pub use types::{
    MeterBlockResponse, MeterBlockTransactions, MeteredPriorityFeeResponse,
    ResourceFeeEstimateResponse,
};

mod transaction;
pub use transaction::{TxValidationError, validate_tx};
