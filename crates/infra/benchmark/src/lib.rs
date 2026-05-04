#![doc = include_str!("../README.md")]

mod config;
pub use config::{
    BenchmarkConfig, BenchmarkDefinition, DatadirConfig, FlashblocksConfig, LoadTestPayloadParams,
    MetricsConfig, MetricsThreshold, SnapshotConfig, TestRun, TransactionPayloadDef, Variable,
    WeightedTx,
};

mod error;
pub use error::BenchmarkError;

mod output;
pub use output::random_id;

mod ports;
pub use ports::PortManager;

mod process;
pub use process::ProcessHandle;

mod snapshots;
pub use snapshots::SnapshotManager;

mod client;
pub use client::{
    setup_node, BaseRethNodeClient, BuilderClient, ClientOptions, ExecutionClient,
    InternalClientOptions,
};

mod consensus;
pub use consensus::{
    BaseConsensusClient, FakeMempool, SequencerConsensusClient, SyncingConsensusClient,
};

mod metrics;
pub use metrics::{
    check_thresholds, write_metrics_json, BlockMetrics, MetricsCollector, Severity,
    ThresholdViolation, GAS_PER_BLOCK, GAS_PER_SECOND, GET_PAYLOAD_LATENCY,
    NEW_PAYLOAD_LATENCY, SEND_TXS_LATENCY, TRANSACTIONS_PER_BLOCK,
    UPDATE_FORK_CHOICE_LATENCY,
};

mod proxy;
pub use proxy::run_proxy;

mod payload;
pub use payload::{LoadTestPayloadWorker, PayloadWorker};

mod flashblocks;
pub use flashblocks::{FlashblockReplayServer, FlashblocksClient};

mod runner;
pub use runner::{NetworkBenchmark, RunResult, RunnerOptions};

mod params;
pub use params::{
    prefund_amount, BATCH_INBOX_ADDRESS, BATCHER_KEY, CHANNEL_TIMEOUT, DEFAULT_GAS_LIMIT,
    EIP1559_DENOMINATOR, EIP1559_ELASTICITY, L1_CHAIN_ID, MAX_SEQUENCER_DRIFT, PREFUND_KEY,
    SEQ_WINDOW_SIZE, SETUP_GAS_LIMIT, SUGGESTED_FEE_RECIPIENT,
};
