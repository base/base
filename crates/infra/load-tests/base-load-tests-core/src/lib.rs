#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod config;
pub use config::{TestConfig, TxTypeConfig, WeightedTxType, WorkloadConfig};

mod utils;
pub use utils::{BaselineError, Result, TracingGuard, init_tracing};

mod rpc;
pub use rpc::{RpcClient, TransactionRequest};

mod metrics;
pub use metrics::{
    GasMetrics, LatencyMetrics, MetricsAggregator, MetricsCollector, MetricsSummary,
    ThroughputMetrics, TransactionMetrics,
};

mod workload;
pub use workload::{
    AccountPool, CalldataPayload, Erc20Payload, FundedAccount, Payload, PrecompilePayload,
    PrecompileTarget, SeededRng, StoragePayload, TransferPayload, UniswapV2Payload,
    UniswapV3Payload, WorkloadGenerator,
};

mod runner;
pub use runner::{
    AdaptiveBackoff, Confirmer, ConfirmerHandle, LoadConfig, LoadRunner, NonceTracker, PendingTx,
    RateLimiter, TxConfig, TxType,
};
