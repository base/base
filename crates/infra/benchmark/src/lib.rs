#![doc = include_str!("../README.md")]

mod config;
pub use config::{
    BenchmarkConfig, BenchmarkDefinition, DatadirConfig, FlashblocksConfig, LoadTestPayloadParams,
    MetricsConfig, MetricsThreshold, SnapshotConfig, TestRun, TransactionPayloadDef, Variable,
    WeightedTx,
};

mod error;
pub use error::BenchmarkError;

mod git;
pub use git::GitInfo;

mod output;
pub use output::{
    RunContext, average_metric, average_metric_seconds, copy_metrics, create_run_dir,
    dump_log_tail, gzip_file, load_payloads_json, random_id, write_metadata_json,
    write_metrics_file, write_payloads_json, write_result_json, write_tags_json,
};

mod ports;
pub use ports::PortManager;

mod process;
pub use process::ProcessHandle;

mod snapshots;
pub use snapshots::SnapshotManager;

mod client;
pub use client::{
    BaseRethNodeClient, BuilderClient, ClientOptions, ExecutionClient, InternalClientOptions,
    setup_node,
};

mod consensus;
pub use consensus::{
    BaseConsensusClient, FakeMempool, SequencerConsensusClient, SyncingConsensusClient,
};

mod metrics;
pub use metrics::{
    BlockMetrics, GAS_PER_BLOCK, GAS_PER_SECOND, GET_PAYLOAD_LATENCY, MetricsCollector,
    NEW_PAYLOAD_LATENCY, SEND_TXS_LATENCY, Severity, TRANSACTIONS_PER_BLOCK, ThresholdViolation,
    UPDATE_FORK_CHOICE_LATENCY, check_thresholds, scrape_prometheus, write_metrics_json,
};

mod proxy;
pub use proxy::run_proxy;

mod payload;
pub use payload::{LoadTestConfig, LoadTestPayloadWorker, PayloadWorker};

mod flashblocks;
pub use flashblocks::{FlashblockReplayServer, FlashblocksClient};

mod runner;
pub use runner::{BenchmarkMode, NetworkBenchmark, RunResult, RunnerOptions};

mod service;
pub use service::{BenchmarkArgs, DEFAULT_CONFIG_YAML, parse_config, run_benchmark};

mod deploy;
pub use deploy::{UniswapV3Addresses, deploy_uniswap_v3};

mod params;
pub use params::{
    BATCH_INBOX_ADDRESS, BATCHER_KEY, CHANNEL_TIMEOUT, DEFAULT_GAS_LIMIT, EIP1559_DENOMINATOR,
    EIP1559_ELASTICITY, L1_CHAIN_ID, MAX_SEQUENCER_DRIFT, PREFUND_ADDRESS, PREFUND_KEY,
    SEQ_WINDOW_SIZE, SETUP_GAS_LIMIT, SUGGESTED_FEE_RECIPIENT, prefund_amount,
};
