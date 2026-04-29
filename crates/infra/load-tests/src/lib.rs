#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::{
    OsakaTarget, PrecompileTarget, TestConfig, TxTypeConfig, WeightedTxType, WorkloadConfig,
};

mod devnet;
pub use devnet::{HARDHAT_TEST_KEYS, devnet_funder, ensure_funder_balance, is_local_rpc};

mod utils;
pub use utils::{BaselineError, Result};

mod rpc;
pub use rpc::{
    BatchRpcClient, BatchSendResult, ReceiptProvider, RpcClient, WalletProvider,
    create_wallet_provider,
};

mod metrics;
pub use metrics::{
    FlashblocksLatencyMetrics, GasMetrics, LatencyMetrics, MetricsAggregator, MetricsCollector,
    MetricsSummary, RollingWindow, ThroughputMetrics, ThroughputPercentiles, TransactionMetrics,
};

mod workload;
pub use workload::{
    AccountPool, AerodromeClPayload, CalldataPayload, Erc20Payload, FundedAccount, OsakaPayload,
    Payload, PrecompileLooper, PrecompilePayload, SeededRng, StoragePayload, TransferPayload,
    UniswapV3Payload, WorkloadGenerator, parse_precompile_id,
};

mod runner;
pub use runner::{
    AdaptiveBackoff, BlockFirstSeen, BlockWatcher, Confirmer, ConfirmerHandle,
    DEFAULT_MAX_GAS_PRICE, DisplaySnapshot, FlashblockTimes, FlashblockTracker, LoadConfig,
    LoadRunner, LoadTestDisplay, RateLimiter, TxConfig, TxType,
};
