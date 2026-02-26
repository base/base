#![doc = include_str!("../README.md")]

/// Health check HTTP server.
mod health;
pub use health::bind_health_server;

/// Prometheus metrics for the ingress RPC service.
mod metrics;
pub use metrics::{Metrics, record_histogram};

/// Kafka message queue publishing.
mod queue;
pub use queue::{BundleQueuePublisher, KafkaMessageQueue, MessageQueue};

/// Core RPC service implementation.
mod service;
pub use service::{IngressApiServer, IngressService, Providers};

/// Transaction validation implementation.
mod validation;
use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
};

use alloy_primitives::TxHash;
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use base_alloy_network::Base;
use base_bundles::MeterBundleResponse;
use base_cli_utils::{LogFormat, LogLevel};
use clap::Parser;
use tokio::sync::broadcast;
use tracing::{error, warn};
use url::Url;
pub use validation::{AccountInfo, AccountInfoLookup, L1BlockInfoLookup, validate_bundle};

/// Method used to submit transactions to the mempool and/or Kafka.
#[derive(Debug, Clone, Copy)]
pub enum TxSubmissionMethod {
    /// Submit via the mempool RPC only.
    Mempool,
    /// Submit via Kafka only.
    Kafka,
    /// Submit via both mempool RPC and Kafka.
    MempoolAndKafka,
    /// Do not submit transactions.
    None,
}

impl FromStr for TxSubmissionMethod {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "mempool" => Ok(Self::Mempool),
            "kafka" => Ok(Self::Kafka),
            "mempool,kafka" | "kafka,mempool" => Ok(Self::MempoolAndKafka),
            "none" => Ok(Self::None),
            _ => Err(format!(
                "Invalid submission method: '{s}'. Valid options: mempool, kafka, mempool,kafka, kafka,mempool, none"
            )),
        }
    }
}

/// Configuration for the tips ingress RPC service.
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Config {
    /// Address to bind the RPC server to
    #[arg(long, env = "TIPS_INGRESS_ADDRESS", default_value = "0.0.0.0")]
    pub address: IpAddr,

    /// Port to bind the RPC server to
    #[arg(long, env = "TIPS_INGRESS_PORT", default_value = "8080")]
    pub port: u16,

    /// URL of the mempool service to proxy transactions to
    #[arg(long, env = "TIPS_INGRESS_RPC_MEMPOOL")]
    pub mempool_url: Url,

    /// Method to submit transactions to the mempool
    #[arg(long, env = "TIPS_INGRESS_TX_SUBMISSION_METHOD", default_value = "mempool")]
    pub tx_submission_method: TxSubmissionMethod,

    /// Kafka brokers for publishing mempool events
    #[arg(long, env = "TIPS_INGRESS_KAFKA_INGRESS_PROPERTIES_FILE")]
    pub ingress_kafka_properties: String,

    /// Kafka topic for queuing transactions before the DB Writer
    #[arg(long, env = "TIPS_INGRESS_KAFKA_INGRESS_TOPIC", default_value = "tips-ingress")]
    pub ingress_topic: String,

    /// Kafka properties file for audit events
    #[arg(long, env = "TIPS_INGRESS_KAFKA_AUDIT_PROPERTIES_FILE")]
    pub audit_kafka_properties: String,

    /// Kafka topic for audit events
    #[arg(long, env = "TIPS_INGRESS_KAFKA_AUDIT_TOPIC", default_value = "tips-audit")]
    pub audit_topic: String,

    /// Log verbosity level
    #[arg(long, env = "TIPS_INGRESS_LOG_LEVEL", default_value = "info")]
    pub log_level: LogLevel,

    /// Log output format (pretty or json)
    #[arg(long, env = "TIPS_INGRESS_LOG_FORMAT", default_value = "pretty")]
    pub log_format: LogFormat,

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

    /// Port to bind the Prometheus metrics server to
    #[arg(long, env = "TIPS_INGRESS_METRICS_ADDR", default_value = "0.0.0.0:9002")]
    pub metrics_addr: SocketAddr,

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

    /// URL of third-party RPC endpoint to forward raw transactions to (enables forwarding if set)
    #[arg(long, env = "TIPS_INGRESS_RAW_TX_FORWARD_RPC")]
    pub raw_tx_forward_rpc: Option<Url>,

    /// TTL for bundle cache in seconds
    #[arg(long, env = "TIPS_INGRESS_BUNDLE_CACHE_TTL", default_value = "20")]
    pub bundle_cache_ttl: u64,

    /// Enable sending to builder
    #[arg(long, env = "TIPS_INGRESS_SEND_TO_BUILDER", default_value = "false")]
    pub send_to_builder: bool,
}

/// Spawns a background task that forwards metering data to the builder RPC.
pub fn connect_ingress_to_builder(
    metering_rx: broadcast::Receiver<MeterBundleResponse>,
    builder_rpc: Url,
) {
    let builder: RootProvider<Base> = ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<Base>()
        .connect_http(builder_rpc);

    tokio::spawn(async move {
        let mut event_rx = metering_rx;
        while let Ok(event) = event_rx.recv().await {
            if event.results.is_empty() {
                warn!(message = "received metering information with no transactions", hash=%event.bundle_hash);
                continue;
            }

            let tx_hash = event.results[0].tx_hash;
            if let Err(e) = builder
                .client()
                .request::<(TxHash, MeterBundleResponse), ()>(
                    "base_setMeteringInformation",
                    (tx_hash, event),
                )
                .await
            {
                error!(error = %e, tx_hash = %tx_hash, "Failed to set metering information");
            }
        }
    });
}
