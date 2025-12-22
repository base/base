#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

// Re-export tips core types
pub use tips_core::types::{Bundle, MeterBundleResponse, TransactionResult};

mod base;
pub use base::{
annotator::{FlashblockInclusion, ResourceAnnotator},
    block::meter_block,
    cache::{BlockMetrics, FlashblockMetrics, MeteredTransaction, MeteringCache, ResourceTotals},
    estimator::{
        BlockPriorityEstimates, EstimateError, FlashblockResourceEstimates, PriorityFeeEstimator,
        ResourceDemand, ResourceEstimate, ResourceEstimates, ResourceKind, ResourceLimits,
        RollingPriorityEstimates,
    },
    meter::meter_bundle,
    meter_rpc::MeteringApiImpl,
    pubsub::{EthPubSub, EthPubSubApiServer},
    traits::{MeteringApiServer, TransactionStatusApiServer},
    transaction_rpc::TransactionStatusApiImpl,
    types::{
        BaseSubscriptionKind, ExtendedSubscriptionKind, MeterBlockResponse, MeterBlockTransactions,
        Status, TransactionStatusResponse,
    },
};

mod eth;
pub use eth::rpc::{EthApiExt, EthApiOverrideServer};

mod metrics;
