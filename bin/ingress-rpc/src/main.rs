//! Ingress RPC binary entry point.

mod cli;

use alloy_provider::ProviderBuilder;
use audit_archiver_lib::{
    AuditConnector, BundleEvent, KafkaBundleEventPublisher, load_kafka_config_from_file,
};
use base_alloy_network::Base;
use base_bundles::MeterBundleResponse;
use base_cli_utils::LogConfig;
use clap::Parser;
use cli::Cli;
use ingress_rpc_lib::{
    BuilderConnector, Config, IngressApiServer, IngressService, KafkaMessageQueue, Providers,
    bind_health_server,
};
use jsonrpsee::server::Server;
use rdkafka::{ClientConfig, producer::FutureProducer};
use tokio::sync::{broadcast, mpsc};
use tracing::info;

base_cli_utils::define_log_args!("TIPS_INGRESS");
base_cli_utils::define_metrics_args!("TIPS_INGRESS", 9002);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    LogConfig::from(cli.log.clone())
        .init_tracing_subscriber()
        .expect("Failed to initialize tracing");

    let metrics_addr = cli.metrics.addr;
    let metrics_port = cli.metrics.port;
    base_cli_utils::MetricsConfig::from(cli.metrics.clone())
        .init()
        .expect("Failed to install Prometheus exporter");

    let config = Config::from(cli);

    info!(
        message = "Starting ingress service",
        address = %config.address,
        port = config.port,
        mempool_url = %config.mempool_url,
        simulation_rpc = %config.simulation_rpc,
        metrics_addr = %metrics_addr,
        metrics_port = metrics_port,
        health_check_address = %config.health_check_addr,
    );

    let providers = Providers {
        mempool: ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Base>()
            .connect_http(config.mempool_url.clone()),
        simulation: ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Base>()
            .connect_http(config.simulation_rpc.clone()),
        raw_tx_forward: config.raw_tx_forward_rpc.clone().map(|url| {
            ProviderBuilder::new().disable_recommended_fillers().network::<Base>().connect_http(url)
        }),
    };

    let ingress_client_config =
        ClientConfig::from_iter(load_kafka_config_from_file(&config.ingress_kafka_properties)?);

    let queue_producer: FutureProducer = ingress_client_config.create()?;

    let queue = KafkaMessageQueue::new(queue_producer);

    let audit_client_config =
        ClientConfig::from_iter(load_kafka_config_from_file(&config.audit_kafka_properties)?);

    let audit_producer: FutureProducer = audit_client_config.create()?;

    let audit_publisher =
        KafkaBundleEventPublisher::new(audit_producer, config.audit_topic.clone());
    let (audit_tx, audit_rx) = mpsc::unbounded_channel::<BundleEvent>();
    AuditConnector::connect(audit_rx, audit_publisher);

    let (builder_tx, _) =
        broadcast::channel::<MeterBundleResponse>(config.max_buffered_meter_bundle_responses);
    config.builder_rpcs.iter().for_each(|builder_rpc| {
        let metering_rx = builder_tx.subscribe();
        BuilderConnector::connect(metering_rx, builder_rpc.clone());
    });

    let health_check_addr = config.health_check_addr;
    let (bound_health_addr, health_handle) = bind_health_server(health_check_addr).await?;
    info!(
        message = "Health check server started",
        address = %bound_health_addr
    );

    let bind_addr = format!("{}:{}", config.address, config.port);
    let service = IngressService::new(providers, queue, audit_tx, builder_tx, config);

    let server = Server::builder().build(&bind_addr).await?;
    let addr = server.local_addr()?;
    let handle = server.start(service.into_rpc());

    info!(
        message = "Ingress RPC server started",
        address = %addr
    );

    handle.stopped().await;
    health_handle.abort();

    Ok(())
}
