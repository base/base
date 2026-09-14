#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::{
    OsakaTarget, PrecompileTarget, PredicateAddressConfig, PredicateSlotConfig,
    PredicateValueConfig, RealTokenAcquisitionConfig, RealTokenPairTokenConfig,
    RealTokenSetupConfig, TestConfig, TxTypeConfig, ValidityConfig, ValidityPredicateConfig,
    WeightedTxType, WorkloadConfig,
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
    BaseFeeExt, BatchRpcClient, BatchSendError, BatchSendResult, JSON_RPC_METHOD_NOT_FOUND,
    MAX_BATCH_RPC_SIZE, QueryProvider, RPC_TIMEOUT, RpcProviders, RpcResultExt, SubmitItem,
    TxpoolAdminClient, WalletProvider, create_wallet_provider,
};

mod metrics;
pub use metrics::{
    BlockLoadMetrics, BlockRange, CohortMetrics, ConfigSummary, FlashblocksLatencyMetrics,
    GasMetrics, LatencyMetrics, MetricsAggregator, MetricsCollector, MetricsSummary,
    PacingCycleObservation, PacingCycleSource, PacingMetrics, ReceiptCoverage, RollingWindow,
    SubmissionStats, SubmitCohortLabel, ThroughputMetrics, ThroughputPercentiles, ThroughputSample,
    TransactionMetrics,
};

mod workload;
pub use workload::{
    AccountPool, AerodromeClPayload, B20TransferPayload, CalldataPayload, ChainPrepContext,
    ChainPrepOutputs, DOUBLE_COUNTER_GAS_LIMIT, DoubleCounterPayload, Erc20Payload, FundedAccount,
    KeyStream, OsakaPayload, Payload, PrecompileLooper, PrecompilePayload, RealTokenAcquisition,
    RealTokenPairTokenSetup, RealTokenRecoverySummary, RealTokenSetup, SeededRng, StoragePayload,
    TransferPayload, UniswapV3Payload, WorkloadGenerator, parse_precompile_id, recover_real_tokens,
};

mod runner;
pub use runner::{
    AdaptiveBackoff, BatchTxError, BlockClock, BlockMatch, BlockNumberBound, BlockObservation,
    BlockPulse, BlockReceipt, BlockWatcher, DEFAULT_MAX_GAS_PRICE,
    DEFAULT_MAX_IN_FLIGHT_PER_SENDER, DisplaySnapshot, FUNDING_MAX_FEE_BASE_FEE_MULTIPLIER, Fees,
    FlashblockInclusion, FlashblockWatcher, GasPricer, InclusionPulse, InclusionSource,
    InjectLimit, InjectPlan, LoadConfig, LoadRunner, LoadTestDisplay, LoadTestStage,
    MAX_FEE_BASE_FEE_MULTIPLIER, MAX_SENDER_WORKER_COUNT, MAX_SIGNER_WORKER_COUNT,
    MIN_PRIORITY_FEE, MeasurementWindow, MempoolDepthController, PipelineQueue,
    PipelineStartConfig, PredicateAddress, PredicateValue, PreparedBatch, PreparedTransaction,
    PresignBuffer, QueuedSubmitFailures, ResultsTracker, SENDER_WORKERS_PER_RPC,
    SIGNER_WORKERS_PER_RPC, SUBMIT_BATCH_QUEUE_BUFFER, SUBMIT_MAX_ATTEMPTS, SenderContext,
    SentTransaction, SignedBatch, SignedTransaction, SignerContext, SlotTemplate,
    SubmissionPipeline, SubmitCohort, SubmitEvent, TxConfig, TxType, ValidityPredicateTemplate,
    ValidityRouter,
};
