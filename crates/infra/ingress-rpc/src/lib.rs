pub mod metrics;
pub mod queue;
pub mod service;
pub mod validation;

use clap::Parser;
use std::net::{IpAddr, SocketAddr};
use url::Url;

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

    /// Enable dual writing raw transactions to the mempool
    #[arg(long, env = "TIPS_INGRESS_DUAL_WRITE_MEMPOOL", default_value = "false")]
    pub dual_write_mempool: bool,

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
}
