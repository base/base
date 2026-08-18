//! Configuration for the tips ingress RPC service.

use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

use base_observability_events::{
    DEFAULT_MAX_FILE_BYTES, DEFAULT_MAX_FILES, DEFAULT_QUEUE_CAPACITY, TransactionEventProducer,
    TransactionEventWriterConfig,
};
use clap::Args;
use url::Url;

/// Configuration for the tips ingress RPC service.
#[derive(Args, Debug, Clone)]
pub struct Config {
    /// Address to bind the RPC server to
    #[arg(long, env = "TIPS_INGRESS_ADDRESS", default_value = "0.0.0.0")]
    pub address: IpAddr,

    /// Port to bind the RPC server to
    #[arg(long, env = "TIPS_INGRESS_PORT", default_value = "8080")]
    pub port: u16,

    /// Deprecated. Ingress no longer proxies transactions to the mempool.
    #[arg(long = "mempool-url", env = "TIPS_INGRESS_RPC_MEMPOOL", hide = true)]
    pub deprecated_mempool_url: Option<Url>,

    /// URL of the audit-archiver RPC endpoint that receives bundle events via
    /// `base_persistBatchedBundleEvent`.
    #[arg(long, env = "TIPS_INGRESS_AUDIT_RPC_URL")]
    pub audit_rpc_url: Url,

    /// Per-request timeout for audit RPC calls, in seconds.
    #[arg(long, env = "TIPS_INGRESS_AUDIT_RPC_TIMEOUT_SECS", default_value = "2")]
    pub audit_rpc_timeout_secs: u64,

    /// Flush the audit batch when it reaches this many events.
    #[arg(long, env = "TIPS_INGRESS_AUDIT_BATCH_MAX_SIZE", default_value = "50")]
    pub audit_batch_max_size: usize,

    /// Maximum time (ms) the first event in a batch waits before forced flush.
    #[arg(long, env = "TIPS_INGRESS_AUDIT_BATCH_MAX_WAIT_MS", default_value = "25")]
    pub audit_batch_max_wait_ms: u64,

    /// Default lifetime for sent transactions in seconds (default: 3 hours)
    #[arg(
        long,
        env = "TIPS_INGRESS_SEND_TRANSACTION_DEFAULT_LIFETIME_SECONDS",
        default_value = "10800"
    )]
    pub send_transaction_default_lifetime_seconds: u64,

    /// URL of the simulation RPC service for bundle metering
    #[arg(long, env = "TIPS_INGRESS_RPC_SIMULATION")]
    pub simulation_rpc: Url,

    /// Configurable block time in milliseconds (default: 2000 milliseconds)
    #[arg(long, env = "TIPS_INGRESS_BLOCK_TIME_MILLISECONDS", default_value = "2000")]
    pub block_time_milliseconds: u64,

    /// Timeout for bundle metering in milliseconds (default: 2000 milliseconds)
    #[arg(long, env = "TIPS_INGRESS_METER_BUNDLE_TIMEOUT_MS", default_value = "2000")]
    pub meter_bundle_timeout_ms: u64,

    /// URLs of the builder RPC service for setting metering information
    #[arg(long, env = "TIPS_INGRESS_BUILDER_RPCS", value_delimiter = ',')]
    pub builder_rpcs: Vec<Url>,

    /// Maximum number of `MeterBundleResponse`s to buffer in memory
    #[arg(long, env = "TIPS_INGRESS_MAX_BUFFERED_METER_BUNDLE_RESPONSES", default_value = "100")]
    pub max_buffered_meter_bundle_responses: usize,

    /// Address to bind the health check server to
    #[arg(long, env = "TIPS_INGRESS_HEALTH_CHECK_ADDR", default_value = "0.0.0.0:8081")]
    pub health_check_addr: SocketAddr,

    /// chain id
    #[arg(long, env = "TIPS_INGRESS_CHAIN_ID", default_value = "11")]
    pub chain_id: u64,

    /// Deprecated. Ingress no longer forwards raw transactions to another RPC endpoint.
    #[arg(long = "raw-tx-forward-rpc", env = "TIPS_INGRESS_RAW_TX_FORWARD_RPC", hide = true)]
    pub deprecated_raw_tx_forward_rpc: Option<Url>,

    /// TTL for bundle cache in seconds
    #[arg(long, env = "TIPS_INGRESS_BUNDLE_CACHE_TTL", default_value = "20")]
    pub bundle_cache_ttl: u64,

    /// Capacity of the bounded audit event channel.
    ///
    /// When the channel is full, new audit events are dropped to avoid blocking
    /// the RPC handler.
    #[arg(long, env = "TIPS_INGRESS_AUDIT_CHANNEL_CAPACITY", default_value = "512")]
    pub audit_channel_capacity: usize,

    /// Enable sending to builder
    #[arg(long, env = "TIPS_INGRESS_SEND_TO_BUILDER", default_value = "false")]
    pub send_to_builder: bool,

    /// Enables transaction observability JSONL journal writes.
    #[arg(long, env = "TIPS_INGRESS_TRANSACTION_EVENTS_ENABLED", default_value = "false")]
    pub transaction_events_enabled: bool,

    /// Dedicated JSONL file path tailed by the transaction-events sidecar.
    #[arg(
        long,
        env = "TIPS_INGRESS_TRANSACTION_EVENTS_FILE_PATH",
        default_value = "/var/log/base/transaction-events.jsonl"
    )]
    pub transaction_events_file_path: PathBuf,

    /// Bounded in-process queue capacity for journal writes.
    #[arg(
        long,
        env = "TIPS_INGRESS_TRANSACTION_EVENTS_QUEUE_CAPACITY",
        default_value_t = DEFAULT_QUEUE_CAPACITY
    )]
    pub transaction_events_queue_capacity: usize,

    /// Maximum size of an active transaction event JSONL segment.
    #[arg(
        long,
        env = "TIPS_INGRESS_TRANSACTION_EVENTS_MAX_FILE_BYTES",
        default_value_t = DEFAULT_MAX_FILE_BYTES
    )]
    pub transaction_events_max_file_bytes: u64,

    /// Maximum number of transaction event JSONL segments to retain, including the active file.
    #[arg(
        long,
        env = "TIPS_INGRESS_TRANSACTION_EVENTS_MAX_FILES",
        default_value_t = DEFAULT_MAX_FILES
    )]
    pub transaction_events_max_files: usize,

    /// Fail service initialization if the journal file cannot be opened.
    #[arg(long, env = "TIPS_INGRESS_TRANSACTION_EVENTS_REQUIRED", default_value = "false")]
    pub transaction_events_required: bool,

    /// Network label written into transaction observability events.
    #[arg(long, env = "TIPS_INGRESS_TRANSACTION_EVENTS_NETWORK", default_value = "base-mainnet")]
    pub transaction_events_network: String,
}

impl Config {
    /// Builds the shared JSONL writer config for ingress transaction events.
    pub fn transaction_event_writer_config(&self) -> TransactionEventWriterConfig {
        TransactionEventWriterConfig {
            enabled: self.transaction_events_enabled,
            file_path: self.transaction_events_file_path.clone(),
            queue_capacity: self.transaction_events_queue_capacity,
            max_file_bytes: self.transaction_events_max_file_bytes,
            max_files: self.transaction_events_max_files,
            required: self.transaction_events_required,
            producer: TransactionEventProducer::IngressRpc,
            network: self.transaction_events_network.clone(),
        }
    }
}
