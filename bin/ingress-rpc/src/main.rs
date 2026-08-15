//! Ingress RPC binary entry point.

use std::time::Duration;

use alloy_provider::RootProvider;
use audit_archiver_lib::{AuditConnector, BundleEvent, RpcBundleEventPublisher};
use base_cli_utils::LogConfig;
use base_common_network::Base;
use base_observability_events::GlobalTransactionEventWriter;
use clap::Parser;
use ingress_rpc_lib::{
    BuilderConnector, Config, HealthServer, IngressApiServer, IngressService,
    MeteringForwardMessage,
};
use jsonrpsee::server::Server;
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

base_cli_utils::define_log_args!("TIPS_INGRESS");
base_cli_utils::define_metrics_args!("TIPS_INGRESS", 9002);

/// CLI entry point for the tips ingress RPC service.
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Service configuration.
    #[command(flatten)]
    config: Config,
    /// Logging configuration.
    #[command(flatten)]
    log: LogArgs,
    /// Metrics configuration.
    #[command(flatten)]
    metrics: MetricsArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();
    let config = cli.config.clone();

    LogConfig::from(cli.log).init_tracing_subscriber().expect("Failed to initialize tracing");

    let metrics_addr = cli.metrics.addr;
    let metrics_port = cli.metrics.port;
    base_cli_utils::MetricsConfig::from(cli.metrics)
        .init()
        .expect("Failed to install Prometheus exporter");

    info!(
        message = "Starting ingress service",
        address = %config.address,
        port = config.port,
        simulation_rpc = %config.simulation_rpc,
        metrics_addr = %metrics_addr,
        metrics_port = metrics_port,
        health_check_address = %config.health_check_addr,
    );

    if config.deprecated_mempool_url.is_some() {
        warn!(
            env = "TIPS_INGRESS_RPC_MEMPOOL",
            "Deprecated ingress mempool forwarding config is set and will be ignored"
        );
    }
    if config.deprecated_raw_tx_forward_rpc.is_some() {
        warn!(
            env = "TIPS_INGRESS_RAW_TX_FORWARD_RPC",
            "Deprecated ingress raw transaction forwarder config is set and will be ignored"
        );
    }

    let simulation_provider = RootProvider::<Base>::new_http(config.simulation_rpc.clone());

    GlobalTransactionEventWriter::init(Some(config.transaction_event_writer_config()))
        .map_err(|err| anyhow::anyhow!("{err:#}"))?;

    let audit_publisher = RpcBundleEventPublisher::new(
        config.audit_rpc_url.as_str(),
        Duration::from_secs(config.audit_rpc_timeout_secs),
    )?;
    let (audit_tx, audit_rx) = mpsc::channel::<BundleEvent>(config.audit_channel_capacity);
    AuditConnector::connect_batched(
        audit_rx,
        audit_publisher,
        config.audit_batch_max_size,
        Duration::from_millis(config.audit_batch_max_wait_ms),
    );

    let (builder_tx, _) =
        broadcast::channel::<MeteringForwardMessage>(config.max_buffered_meter_bundle_responses);
    info!(
        builder_rpcs = ?config.builder_rpcs,
        send_to_builder = config.send_to_builder,
        "Configuring builder connectors"
    );
    config.builder_rpcs.iter().enumerate().for_each(|(destination_index, builder_rpc)| {
        let metering_rx = builder_tx.subscribe();
        BuilderConnector::connect(metering_rx, builder_rpc.clone(), destination_index);
    });

    let health_check_addr = config.health_check_addr;
    let (bound_health_addr, health_handle) = HealthServer::bind(health_check_addr).await?;
    info!(
        message = "Health check server started",
        address = %bound_health_addr
    );

    let bind_addr = format!("{}:{}", config.address, config.port);
    let service = IngressService::new(simulation_provider, audit_tx, builder_tx, cli.config);

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
