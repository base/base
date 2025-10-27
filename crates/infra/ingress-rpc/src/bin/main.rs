use alloy_provider::{ProviderBuilder, RootProvider};
use clap::Parser;
use jsonrpsee::server::Server;
use op_alloy_network::Optimism;
use rdkafka::ClientConfig;
use rdkafka::producer::FutureProducer;
use std::net::IpAddr;
use tips_core::kafka::load_kafka_config_from_file;
use tips_core::logger::init_logger;
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

    #[arg(long, env = "TIPS_INGRESS_LOG_LEVEL", default_value = "info")]
    log_level: String,

    /// Default lifetime for sent transactions in seconds (default: 3 hours)
    #[arg(
        long,
        env = "TIPS_INGRESS_SEND_TRANSACTION_DEFAULT_LIFETIME_SECONDS",
        default_value = "10800"
    )]
    send_transaction_default_lifetime_seconds: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let config = Config::parse();

    init_logger(&config.log_level);

    info!(
        message = "Starting ingress service",
        address = %config.address,
        port = config.port,
        mempool_url = %config.mempool_url
    );

    let provider: RootProvider<Optimism> = ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<Optimism>()
        .connect_http(config.mempool_url);

    let client_config = ClientConfig::from_iter(load_kafka_config_from_file(
        &config.ingress_kafka_properties,
    )?);

    let queue_producer: FutureProducer = client_config.create()?;

    let queue = KafkaQueuePublisher::new(queue_producer, config.ingress_topic);

    let service = IngressService::new(
        provider,
        config.dual_write_mempool,
        queue,
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
