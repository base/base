#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::{
    OsakaTarget, PrecompileTarget, RealTokenAcquisitionConfig, RealTokenPairTokenConfig,
    RealTokenSetupConfig, TestConfig, TxTypeConfig, WeightedTxType, WorkloadConfig,
};

mod executor;
pub use executor::{
    LoadTestCleanupSummary, LoadTestDisplayConfig, LoadTestExecutor, LoadTestRunHooks,
    LoadTestRunOptions, LoadTestRunOutput, LoadTestSetupAmounts, SignalHandlerGuard,
};

mod utils;
pub use utils::{BaselineError, Result};

mod rpc;
pub use rpc::{
    BaseFeeExt, BatchRpcClient, BatchSendResult, QueryProvider, RPC_TIMEOUT, RpcProviders,
    RpcResultExt, TxpoolAdminClient, WalletProvider, create_wallet_provider,
};

mod metrics;
pub use metrics::{
    BlockLoadMetrics, BlockRange, ConfigSummary, FlashblocksLatencyMetrics, GasMetrics,
    LatencyMetrics, MetricsAggregator, MetricsCollector, MetricsSummary, ReceiptCoverage,
    RollingWindow, SubmissionStats, ThroughputMetrics, ThroughputPercentiles, ThroughputSample,
    TransactionMetrics,
};

mod workload;
pub use workload::{
    AccountPool, AerodromeClPayload, B20TransferPayload, CalldataPayload, Erc20Payload,
    FundedAccount, KeyStream, OsakaPayload, Payload, PrecompileLooper, PrecompilePayload,
    SeededRng, StoragePayload, TransferPayload, UniswapV3Payload, WorkloadGenerator,
    parse_precompile_id,
};

mod runner;
pub use runner::{
    AdaptiveBackoff, BatchTxError, BlockObservation, BlockReceipt, BlockWatcher,
    DEFAULT_MAX_GAS_PRICE, DisplaySnapshot, FlashblockInclusion, FlashblockWatcher, LoadConfig,
    LoadRunner, LoadTestDisplay, MAX_FEE_BASE_FEE_MULTIPLIER, MAX_SENDER_WORKER_COUNT,
    MAX_SIGNER_WORKER_COUNT, PipelineQueue, PreparedBatch, PreparedTransaction,
    QueuedSubmitFailures, RateLimiter, RealTokenAcquisition, RealTokenPairTokenSetup,
    RealTokenRecoverySummary, RealTokenSetup, ResultsTracker, SENDER_WORKERS_PER_RPC,
    SIGNER_WORKERS_PER_RPC, SUBMIT_BATCH_QUEUE_BUFFER, SUBMIT_MAX_ATTEMPTS, SenderContext,
    SentTransaction, SignedBatch, SignedTransaction, SignerContext, SubmissionPipeline,
    SubmitEvent, TxConfig, TxType,
};
