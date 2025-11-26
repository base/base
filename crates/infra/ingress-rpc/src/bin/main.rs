use alloy_provider::{ProviderBuilder, RootProvider};
use clap::Parser;
use jsonrpsee::server::Server;
use op_alloy_network::Optimism;
use rdkafka::ClientConfig;
use rdkafka::producer::FutureProducer;
use tips_audit::{BundleEvent, KafkaBundleEventPublisher, connect_audit_to_publisher};
use tips_core::MeterBundleResponse;
use tips_core::kafka::load_kafka_config_from_file;
use tips_core::logger::init_logger_with_format;
use tips_ingress_rpc::Config;
use tips_ingress_rpc::connect_ingress_to_builder;
use tips_ingress_rpc::metrics::init_prometheus_exporter;
use tips_ingress_rpc::queue::KafkaQueuePublisher;
use tips_ingress_rpc::service::{IngressApiServer, IngressService};
use tokio::sync::{broadcast, mpsc};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let config = Config::parse();
    // clone once instead of cloning each field before passing to `IngressService::new`
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
    let (audit_tx, audit_rx) = mpsc::unbounded_channel::<BundleEvent>();
    connect_audit_to_publisher(audit_rx, audit_publisher);

    let (builder_tx, _) =
        broadcast::channel::<MeterBundleResponse>(config.max_buffered_meter_bundle_responses);
    config.builder_rpcs.iter().for_each(|builder_rpc| {
        let builder_rx = builder_tx.subscribe();
        connect_ingress_to_builder(builder_rx, builder_rpc.clone());
    });

    let service = IngressService::new(
        provider,
        simulation_provider,
        queue,
        audit_tx,
        builder_tx,
        cfg,
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
