#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::{PrecompileTarget, TestConfig, TxTypeConfig, WeightedTxType, WorkloadConfig};

mod utils;
pub use utils::{BaselineError, Result, init_tracing};

mod rpc;
pub use rpc::{ReceiptProvider, RpcClient, WalletProvider, create_wallet_provider};

mod metrics;
pub use metrics::{
    GasMetrics, LatencyMetrics, MetricsAggregator, MetricsCollector, MetricsSummary, RollingWindow,
    ThroughputMetrics, TransactionMetrics,
};

mod workload;
pub use workload::{
    AccountPool, CalldataPayload, Erc20Payload, FundedAccount, Payload, PrecompileLooper,
    PrecompilePayload, SeededRng, StoragePayload, TransferPayload, UniswapV2Payload,
    UniswapV3Payload, WorkloadGenerator, parse_precompile_id,
};

mod runner;
pub use runner::{
    AdaptiveBackoff, Confirmer, ConfirmerHandle, DEFAULT_MAX_GAS_PRICE, DisplaySnapshot,
    LoadConfig, LoadRunner, LoadTestDisplay, RateLimiter, TxConfig, TxType,
};
