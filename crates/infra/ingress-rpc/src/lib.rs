pub mod metrics;
pub mod queue;
pub mod service;
pub mod validation;

use alloy_primitives::TxHash;
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use clap::Parser;
use op_alloy_network::Optimism;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use tips_core::MeterBundleResponse;
use tokio::sync::broadcast;
use tracing::error;
use url::Url;

#[derive(Debug, Clone, Copy)]
pub enum TxSubmissionMethod {
    Mempool,
    Kafka,
    MempoolAndKafka,
}

impl FromStr for TxSubmissionMethod {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "mempool" => Ok(TxSubmissionMethod::Mempool),
            "kafka" => Ok(TxSubmissionMethod::Kafka),
            "mempool,kafka" | "kafka,mempool" => Ok(TxSubmissionMethod::MempoolAndKafka),
            _ => Err(format!(
                "Invalid submission method: '{s}'. Valid options: mempool, kafka, mempool,kafka, kafka,mempool"
            )),
        }
    }
}

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
    #[arg(
        long,
        env = "TIPS_INGRESS_TX_SUBMISSION_METHOD",
        default_value = "mempool"
    )]
    pub tx_submission_method: TxSubmissionMethod,

    /// Kafka brokers for publishing mempool events
    #[arg(long, env = "TIPS_INGRESS_KAFKA_INGRESS_PROPERTIES_FILE")]
    pub ingress_kafka_properties: String,

    /// Kafka topic for queuing transactions before the DB Writer
    #[arg(
        long,
        env = "TIPS_INGRESS_KAFKA_INGRESS_TOPIC",
        default_value = "tips-ingress"
    )]
    pub ingress_topic: String,

    /// Kafka properties file for audit events
    #[arg(long, env = "TIPS_INGRESS_KAFKA_AUDIT_PROPERTIES_FILE")]
    pub audit_kafka_properties: String,

    /// Kafka topic for audit events
    #[arg(
        long,
        env = "TIPS_INGRESS_KAFKA_AUDIT_TOPIC",
        default_value = "tips-audit"
    )]
    pub audit_topic: String,

    #[arg(long, env = "TIPS_INGRESS_LOG_LEVEL", default_value = "info")]
    pub log_level: String,

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
    #[arg(
        long,
        env = "TIPS_INGRESS_METRICS_ADDR",
        default_value = "0.0.0.0:9002"
    )]
    pub metrics_addr: SocketAddr,

    /// Configurable block time in milliseconds (default: 2000 milliseconds)
    #[arg(
        long,
        env = "TIPS_INGRESS_BLOCK_TIME_MILLISECONDS",
        default_value = "2000"
    )]
    pub block_time_milliseconds: u64,

    /// Timeout for bundle metering in milliseconds (default: 2000 milliseconds)
    #[arg(
        long,
        env = "TIPS_INGRESS_METER_BUNDLE_TIMEOUT_MS",
        default_value = "2000"
    )]
    pub meter_bundle_timeout_ms: u64,

    /// URLs of the builder RPC service for setting metering information
    #[arg(long, env = "TIPS_INGRESS_BUILDER_RPCS")]
    pub builder_rpcs: Vec<Url>,

    /// Maximum number of `MeterBundleResponse`s to buffer in memory
    #[arg(
        long,
        env = "TIPS_INGRESS_MAX_BUFFERED_METER_BUNDLE_RESPONSES",
        default_value = "100"
    )]
    pub max_buffered_meter_bundle_responses: usize,
}

pub fn connect_ingress_to_builder(
    event_rx: broadcast::Receiver<MeterBundleResponse>,
    builder_rpc: Url,
) {
    tokio::spawn(async move {
        let builder: RootProvider<Optimism> = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Optimism>()
            .connect_http(builder_rpc);

        let mut event_rx = event_rx;
        while let Ok(event) = event_rx.recv().await {
            // we only support one transaction per bundle for now
            let tx_hash = event.results[0].tx_hash;
            if let Err(e) = builder
                .client()
                .request::<(TxHash, MeterBundleResponse), ()>(
                    "base_setMeteringInformation",
                    (tx_hash, event),
                )
                .await
            {
                error!(error = %e, "Failed to set metering information for tx hash: {tx_hash}");
            }
        }
    });
}
