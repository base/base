#![doc = include_str!("../README.md")]

mod config;
pub use config::{
    BenchmarkConfig, BenchmarkDefinition, DatadirConfig, FlashblocksConfig, LoadTestPayloadParams,
    MetricsConfig, MetricsThreshold, SetupConfig, SnapshotConfig, TestRun, TransactionPayloadDef,
    Variable, WeightedTx,
};

mod error;
pub use error::BenchmarkError;

mod git;
pub use git::GitInfo;

mod output;
pub use output::{
    average_metric, average_metric_seconds, copy_metrics, create_run_dir, dump_log_tail,
    gzip_file, random_id, write_metadata_json, write_metrics_file, write_result_json,
    write_tags_json, ResultsIndexEntry, RunContext,
};

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
    check_thresholds, scrape_prometheus, write_metrics_json, BlockMetrics, MetricsCollector,
    Severity, ThresholdViolation, GAS_PER_BLOCK, GAS_PER_SECOND, GET_PAYLOAD_LATENCY,
    NEW_PAYLOAD_LATENCY, SEND_TXS_LATENCY, TRANSACTIONS_PER_BLOCK,
    UPDATE_FORK_CHOICE_LATENCY,
};

mod proxy;
pub use proxy::run_proxy;

mod payload;
pub use payload::{LoadTestConfig, LoadTestPayloadWorker, PayloadWorker};

mod flashblocks;
pub use flashblocks::{FlashblockReplayServer, FlashblocksClient};

mod runner;
pub use runner::{NetworkBenchmark, RunResult, RunnerOptions};

mod service;
pub use service::{parse_config, run_benchmark, BenchmarkArgs, DEFAULT_CONFIG_YAML};

mod deploy;
pub use deploy::{deploy_uniswap_v3, UniswapV3Addresses};

mod params;
pub use params::{
    prefund_amount, BATCH_INBOX_ADDRESS, BATCHER_KEY, CHANNEL_TIMEOUT, DEFAULT_GAS_LIMIT,
    EIP1559_DENOMINATOR, EIP1559_ELASTICITY, L1_CHAIN_ID, MAX_SEQUENCER_DRIFT, PREFUND_KEY,
    SEQ_WINDOW_SIZE, SETUP_GAS_LIMIT, SUGGESTED_FEE_RECIPIENT,
};
