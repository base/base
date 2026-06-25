#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use alloy_primitives::TxHash;

pub(crate) const DEFAULT_PENDING_STATE_ROOT_TIMES_CAPACITY: usize = 10_000;
pub(crate) type PendingStateRootTimes = lru::LruCache<TxHash, u128>;

mod collector;
pub use collector::MeteringCollector;

mod block;
pub use block::meter_block;

mod cache;
pub use cache::{
    BlockMetrics, FlashblockMetrics, MeteredTransaction, MeteringCache, ResourceTotals,
};

mod estimator;
pub use estimator::{
    BlockPriorityEstimates, EstimateError, FlashblockResourceEstimates, PriorityFeeEstimator,
    ResourceDemand, ResourceEstimate, ResourceEstimates, ResourceKind, ResourceLimits,
    RollingPriorityEstimate,
};

mod extension;
pub use extension::{MeteringConfig, MeteringExtension, MeteringResourceLimits};

mod inspector;

mod meter;
pub use meter::{
    MeterBundleInput, MeterBundleOutput, MeteredOpcodes, PendingState, PendingTrieInput,
    meter_bundle,
};

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
