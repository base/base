//! Tips ingress RPC binary entry point.

use alloy_provider::ProviderBuilder;
use clap::Parser;
use jsonrpsee::server::Server;
use op_alloy_network::Optimism;
use rdkafka::{ClientConfig, producer::FutureProducer};
use tips_audit_lib::{BundleEvent, KafkaBundleEventPublisher, connect_audit_to_publisher};
use tips_core::{
    AcceptedBundle, MeterBundleResponse, kafka::load_kafka_config_from_file,
    logger::init_logger_with_format, metrics::init_prometheus_exporter,
};
use tips_ingress_rpc_lib::{
    Config, IngressApiServer, IngressService, KafkaMessageQueue, Providers, bind_health_server,
    connect_ingress_to_builder,
};
use tokio::sync::{broadcast, mpsc};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let config = Config::parse();
    let cfg = config.clone();

    init_logger_with_format(&config.log_level, config.log_format);

    init_prometheus_exporter(config.metrics_addr).expect("Failed to install Prometheus exporter");

    info!(
        message = "Starting ingress service",
        address = %config.address,
        port = config.port,
        mempool_url = %config.mempool_url,
        simulation_rpc = %config.simulation_rpc,
        metrics_address = %config.metrics_addr,
        health_check_address = %config.health_check_addr,
    );

    let providers = Providers {
        mempool: ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Optimism>()
            .connect_http(config.mempool_url),
        simulation: ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Optimism>()
            .connect_http(config.simulation_rpc),
        raw_tx_forward: config.raw_tx_forward_rpc.clone().map(|url| {
            ProviderBuilder::new()
                .disable_recommended_fillers()
                .network::<Optimism>()
                .connect_http(url)
        }),
    };

    let ingress_client_config =
        ClientConfig::from_iter(load_kafka_config_from_file(&config.ingress_kafka_properties)?);

    let queue_producer: FutureProducer = ingress_client_config.create()?;

    let queue = KafkaMessageQueue::new(queue_producer);

    let audit_client_config =
        ClientConfig::from_iter(load_kafka_config_from_file(&config.audit_kafka_properties)?);

    let audit_producer: FutureProducer = audit_client_config.create()?;

    let audit_publisher = KafkaBundleEventPublisher::new(audit_producer, config.audit_topic);
    let (audit_tx, audit_rx) = mpsc::unbounded_channel::<BundleEvent>();
    connect_audit_to_publisher(audit_rx, audit_publisher);

    let (builder_tx, _) =
        broadcast::channel::<MeterBundleResponse>(config.max_buffered_meter_bundle_responses);
    let (builder_backrun_tx, _) =
        broadcast::channel::<AcceptedBundle>(config.max_buffered_backrun_bundles);
    config.builder_rpcs.iter().for_each(|builder_rpc| {
        let metering_rx = builder_tx.subscribe();
        let backrun_rx = builder_backrun_tx.subscribe();
        connect_ingress_to_builder(metering_rx, backrun_rx, builder_rpc.clone());
    });

    let health_check_addr = config.health_check_addr;
    let (bound_health_addr, health_handle) = bind_health_server(health_check_addr).await?;
    info!(
        message = "Health check server started",
        address = %bound_health_addr
    );

    let service =
        IngressService::new(providers, queue, audit_tx, builder_tx, builder_backrun_tx, cfg);
    let bind_addr = format!("{}:{}", config.address, config.port);

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
