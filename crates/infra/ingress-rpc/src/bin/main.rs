use alloy_provider::{ProviderBuilder, RootProvider};
use clap::Parser;
use jsonrpsee::server::Server;
use op_alloy_network::Optimism;
use rdkafka::ClientConfig;
use rdkafka::producer::FutureProducer;
use std::net::{IpAddr, SocketAddr};
use tips_audit::KafkaBundleEventPublisher;
use tips_core::kafka::load_kafka_config_from_file;
use tips_core::logger::init_logger;
use tips_ingress_rpc::metrics::init_prometheus_exporter;
use tips_ingress_rpc::queue::KafkaQueuePublisher;
use tips_ingress_rpc::service::{IngressApiServer, IngressService};
use tracing::info;
use url::Url;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Config {
    /// Address to bind the RPC server to
    #[arg(long, env = "TIPS_INGRESS_ADDRESS", default_value = "0.0.0.0")]
    address: IpAddr,

    /// Port to bind the RPC server to
    #[arg(long, env = "TIPS_INGRESS_PORT", default_value = "8080")]
    port: u16,

    /// URL of the mempool service to proxy transactions to
    #[arg(long, env = "TIPS_INGRESS_RPC_MEMPOOL")]
    mempool_url: Url,

    /// Enable dual writing raw transactions to the mempool
    #[arg(long, env = "TIPS_INGRESS_DUAL_WRITE_MEMPOOL", default_value = "false")]
    dual_write_mempool: bool,

    /// Kafka brokers for publishing mempool events
    #[arg(long, env = "TIPS_INGRESS_KAFKA_INGRESS_PROPERTIES_FILE")]
    ingress_kafka_properties: String,

    /// Kafka topic for queuing transactions before the DB Writer
    #[arg(
        long,
        env = "TIPS_INGRESS_KAFKA_INGRESS_TOPIC",
        default_value = "tips-ingress"
    )]
    ingress_topic: String,

    /// Kafka properties file for audit events
    #[arg(long, env = "TIPS_INGRESS_KAFKA_AUDIT_PROPERTIES_FILE")]
    audit_kafka_properties: String,

    /// Kafka topic for audit events
    #[arg(
        long,
        env = "TIPS_INGRESS_KAFKA_AUDIT_TOPIC",
        default_value = "tips-audit"
    )]
    audit_topic: String,

    #[arg(long, env = "TIPS_INGRESS_LOG_LEVEL", default_value = "info")]
    log_level: String,

    /// Default lifetime for sent transactions in seconds (default: 3 hours)
    #[arg(
        long,
        env = "TIPS_INGRESS_SEND_TRANSACTION_DEFAULT_LIFETIME_SECONDS",
        default_value = "10800"
    )]
    send_transaction_default_lifetime_seconds: u64,

    /// URL of the simulation RPC service for bundle metering
    #[arg(long, env = "TIPS_INGRESS_RPC_SIMULATION")]
    simulation_rpc: Url,

    /// Port to bind the Prometheus metrics server to
    #[arg(
        long,
        env = "TIPS_INGRESS_METRICS_ADDR",
        default_value = "0.0.0.0:9002"
    )]
    metrics_addr: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let config = Config::parse();

    init_logger(&config.log_level);

    init_prometheus_exporter(config.metrics_addr).expect("Failed to install Prometheus exporter");

    info!(
        message = "Starting ingress service",
        address = %config.address,
        port = config.port,
        mempool_url = %config.mempool_url,
        simulation_rpc = %config.simulation_rpc,
        metrics_address = %config.metrics_addr,
    );

    let provider: RootProvider<Optimism> = ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<Optimism>()
        .connect_http(config.mempool_url);

    let simulation_provider: RootProvider<Optimism> = ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<Optimism>()
        .connect_http(config.simulation_rpc);

    let ingress_client_config = ClientConfig::from_iter(load_kafka_config_from_file(
        &config.ingress_kafka_properties,
    )?);

    let queue_producer: FutureProducer = ingress_client_config.create()?;

    let queue = KafkaQueuePublisher::new(queue_producer, config.ingress_topic);

    let audit_client_config =
        ClientConfig::from_iter(load_kafka_config_from_file(&config.audit_kafka_properties)?);

    let audit_producer: FutureProducer = audit_client_config.create()?;

    let audit_publisher = KafkaBundleEventPublisher::new(audit_producer, config.audit_topic);

    let service = IngressService::new(
        provider,
        simulation_provider,
        config.dual_write_mempool,
        queue,
        audit_publisher,
        config.send_transaction_default_lifetime_seconds,
    );
    let bind_addr = format!("{}:{}", config.address, config.port);

    let server = Server::builder().build(&bind_addr).await?;
    let addr = server.local_addr()?;
    let handle = server.start(service.into_rpc());

    info!(
        message = "Ingress RPC server started",
        address = %addr
    );

    handle.stopped().await;
    Ok(())
}
