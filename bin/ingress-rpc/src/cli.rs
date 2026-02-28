use std::net::{IpAddr, SocketAddr};

use clap::Parser;
use ingress_rpc_lib::{Config, TxSubmissionMethod};
use url::Url;

use crate::{LogArgs, MetricsArgs};

/// CLI entry point for the tips ingress RPC service.
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub(crate) struct Cli {
    /// Address to bind the RPC server to
    #[arg(long = "address", id = "ingress_address", env = "TIPS_INGRESS_ADDRESS", default_value = "0.0.0.0")]
    pub address: IpAddr,

    /// Port to bind the RPC server to
    #[arg(long = "port", id = "ingress_port", env = "TIPS_INGRESS_PORT", default_value = "8080")]
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

    /// URL of third-party RPC endpoint to forward raw transactions to (enables forwarding if set)
    #[arg(long, env = "TIPS_INGRESS_RAW_TX_FORWARD_RPC")]
    pub raw_tx_forward_rpc: Option<Url>,

    /// TTL for bundle cache in seconds
    #[arg(long, env = "TIPS_INGRESS_BUNDLE_CACHE_TTL", default_value = "20")]
    pub bundle_cache_ttl: u64,

    /// Enable sending to builder
    #[arg(long, env = "TIPS_INGRESS_SEND_TO_BUILDER", default_value = "false")]
    pub send_to_builder: bool,

    /// Logging configuration.
    #[command(flatten)]
    pub log: LogArgs,

    /// Metrics configuration.
    #[command(flatten)]
    pub metrics: MetricsArgs,
}

impl From<Cli> for Config {
    fn from(cli: Cli) -> Self {
        Self {
            address: cli.address,
            port: cli.port,
            mempool_url: cli.mempool_url,
            tx_submission_method: cli.tx_submission_method,
            ingress_kafka_properties: cli.ingress_kafka_properties,
            ingress_topic: cli.ingress_topic,
            audit_kafka_properties: cli.audit_kafka_properties,
            audit_topic: cli.audit_topic,
            send_transaction_default_lifetime_seconds: cli.send_transaction_default_lifetime_seconds,
            simulation_rpc: cli.simulation_rpc,
            block_time_milliseconds: cli.block_time_milliseconds,
            meter_bundle_timeout_ms: cli.meter_bundle_timeout_ms,
            builder_rpcs: cli.builder_rpcs,
            max_buffered_meter_bundle_responses: cli.max_buffered_meter_bundle_responses,
            health_check_addr: cli.health_check_addr,
            chain_id: cli.chain_id,
            raw_tx_forward_rpc: cli.raw_tx_forward_rpc,
            bundle_cache_ttl: cli.bundle_cache_ttl,
            send_to_builder: cli.send_to_builder,
        }
    }
}
