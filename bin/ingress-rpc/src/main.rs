//! Ingress RPC binary entry point.

use std::time::Duration;

use alloy_provider::RootProvider;
use audit_archiver_lib::{AuditConnector, BundleEvent, RpcBundleEventPublisher};
use base_cli_utils::LogConfig;
use base_common_network::Base;
use base_observability_events::{DEFAULT_SHUTDOWN_TIMEOUT, GlobalTransactionEventWriter};
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
    let _transaction_event_journal =
        GlobalTransactionEventWriter::drain_on_drop(DEFAULT_SHUTDOWN_TIMEOUT);

    run_service(config, simulation_provider).await
}

async fn run_service(
    config: Config,
    simulation_provider: RootProvider<Base>,
) -> anyhow::Result<()> {
    let mut health_handle = None;
    let mut builder_handles = Vec::new();
    let mut audit_handle = None;

    let result = serve(
        config,
        simulation_provider,
        &mut health_handle,
        &mut builder_handles,
        &mut audit_handle,
    )
    .await;

    join_or_abort_with_timeout(
        health_handle,
        builder_handles,
        audit_handle,
        DEFAULT_SHUTDOWN_TIMEOUT,
    )
    .await;

    result
}

async fn serve(
    config: Config,
    simulation_provider: RootProvider<Base>,
    health_handle: &mut Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    builder_handles: &mut Vec<tokio::task::JoinHandle<()>>,
    audit_handle: &mut Option<tokio::task::JoinHandle<()>>,
) -> anyhow::Result<()> {
    let audit_publisher = RpcBundleEventPublisher::new(
        config.audit_rpc_url.as_str(),
        Duration::from_secs(config.audit_rpc_timeout_secs),
    )?;
    let (audit_tx, audit_rx) = mpsc::channel::<BundleEvent>(config.audit_channel_capacity);
    *audit_handle = Some(AuditConnector::connect_batched(
        audit_rx,
        audit_publisher,
        config.audit_batch_max_size,
        Duration::from_millis(config.audit_batch_max_wait_ms),
    ));

    let (builder_tx, _) =
        broadcast::channel::<MeteringForwardMessage>(config.max_buffered_meter_bundle_responses);
    info!(
        builder_rpcs = ?config.builder_rpcs,
        send_to_builder = config.send_to_builder,
        "Configuring builder connectors"
    );
    *builder_handles = config
        .builder_rpcs
        .iter()
        .enumerate()
        .map(|(destination_index, builder_rpc)| {
            BuilderConnector::connect(
                builder_tx.subscribe(),
                builder_rpc.clone(),
                destination_index,
            )
        })
        .collect();

    let health_check_addr = config.health_check_addr;
    let (bound_health_addr, handle) = HealthServer::bind(health_check_addr).await?;
    *health_handle = Some(handle);
    info!(
        message = "Health check server started",
        address = %bound_health_addr
    );

    let bind_addr = format!("{}:{}", config.address, config.port);
    let service = IngressService::new(simulation_provider, audit_tx, builder_tx, config);

    let server = Server::builder().build(&bind_addr).await?;
    let addr = server.local_addr()?;
    let handle = server.start(service.into_rpc());

    info!(
        message = "Ingress RPC server started",
        address = %addr
    );

    tokio::select! {
        () = handle.clone().stopped() => {
            info!("Ingress RPC server stopped");
        }
        signal = shutdown_signal() => {
            info!(signal, "shutdown signal received, stopping ingress RPC server");
            if let Err(err) = handle.stop() {
                warn!(error = %err, "ingress RPC server already stopped");
            }
            handle.stopped().await;
        }
    }

    Ok(())
}

async fn join_or_abort_with_timeout(
    health_handle: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    builder_handles: Vec<tokio::task::JoinHandle<()>>,
    audit_handle: Option<tokio::task::JoinHandle<()>>,
    timeout: Duration,
) {
    if health_handle.is_none() && builder_handles.is_empty() && audit_handle.is_none() {
        return;
    }

    let health_abort = health_handle.as_ref().map(tokio::task::JoinHandle::abort_handle);
    let builder_aborts: Vec<_> =
        builder_handles.iter().map(tokio::task::JoinHandle::abort_handle).collect();
    let audit_abort = audit_handle.as_ref().map(tokio::task::JoinHandle::abort_handle);

    if let Some(handle) = &health_handle {
        handle.abort();
    }

    let join = async move {
        if let Some(handle) = health_handle
            && let Err(err) = handle.await
            && !err.is_cancelled()
        {
            warn!(error = %err, "health check server task ended with error");
        }
        for handle in builder_handles {
            if let Err(err) = handle.await
                && !err.is_cancelled()
            {
                warn!(error = %err, "builder connector task ended with error");
            }
        }
        if let Some(handle) = audit_handle
            && let Err(err) = handle.await
            && !err.is_cancelled()
        {
            warn!(error = %err, "audit connector task ended with error");
        }
    };
    tokio::pin!(join);

    tokio::select! {
        _ = &mut join => {}
        _ = tokio::time::sleep(timeout) => {
            warn!("ingress background tasks exceeded shutdown budget; aborting");
            if let Some(abort) = health_abort {
                abort.abort();
            }
            for abort in builder_aborts {
                abort.abort();
            }
            if let Some(abort) = audit_abort {
                abort.abort();
            }
            if tokio::time::timeout(timeout, join).await.is_err() {
                warn!("ingress background tasks did not exit after abort");
            }
        }
    }
}

/// Wait for a graceful-shutdown signal.
///
/// On Unix this races `SIGTERM` (the default signal Kubernetes sends on pod
/// shutdown) and `SIGINT` (Ctrl-C). On other platforms it falls back to Ctrl-C.
#[cfg(unix)]
async fn shutdown_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => "SIGTERM",
        _ = sigint.recv() => "SIGINT",
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "ctrl_c"
}
