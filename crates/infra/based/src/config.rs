use std::net::UdpSocket;

use base_cli_utils::LogConfig;
use cadence::{StatsdClient, UdpMetricSink};
use tracing::info;

use crate::{
    BlockProductionHealthChecker, HealthcheckConfig, HealthcheckMetrics, Node,
    alloy_client::AlloyEthClient,
};

/// Top-level configuration for the `based` healthcheck service.
///
/// This struct is clap-free so the library can be driven from tests,
/// config files, or any other source.
#[derive(Debug, Clone)]
pub struct BasedConfig {
    /// Ethereum node HTTP RPC URL.
    pub node_url: String,
    /// Treat the node as a new instance on startup (suppresses initial errors until healthy).
    pub new_instance: bool,
    /// Health-check timing parameters.
    pub healthcheck: HealthcheckConfig,
    /// Logging configuration.
    pub log: LogConfig,
}

/// Runs the block-production healthcheck sidecar to completion.
///
/// Sets up a `StatsD` metrics client, creates the Ethereum RPC client, and
/// polls the node for health checks until a Ctrl+C shutdown signal is received.
pub async fn run(config: BasedConfig) -> anyhow::Result<()> {
    config
        .log
        .init_tracing_subscriber()
        .map_err(|e| anyhow::anyhow!("{e:#}"))?;

    // Initialize StatsD client (sends to Datadog agent).
    // Use DD_AGENT_HOST if set (Kubernetes), otherwise localhost.
    let statsd_host = std::env::var("DD_AGENT_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let statsd_addr = format!("{statsd_host}:8125");
    info!(address = %statsd_addr, "connecting to StatsD agent");

    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_nonblocking(true)?;
    let sink = UdpMetricSink::from(statsd_addr.as_str(), socket)?;

    // Read tags from CODEFLOW environment variables.
    let config_name =
        std::env::var("CODEFLOW_CONFIG_NAME").unwrap_or_else(|_| "unknown".to_string());
    let environment =
        std::env::var("CODEFLOW_ENVIRONMENT").unwrap_or_else(|_| "unknown".to_string());
    let project_name =
        std::env::var("CODEFLOW_PROJECT_NAME").unwrap_or_else(|_| "unknown".to_string());
    let service_name =
        std::env::var("CODEFLOW_SERVICE_NAME").unwrap_or_else(|_| "unknown".to_string());

    let statsd_client = StatsdClient::builder("base.blocks", sink)
        .with_tag("configname", &config_name)
        .with_tag("environment", &environment)
        .with_tag("projectname", &project_name)
        .with_tag("servicename", &service_name)
        .build();

    info!(
        configname = %config_name,
        environment = %environment,
        projectname = %project_name,
        servicename = %service_name,
        "initialized StatsD client with tags"
    );

    let metrics = HealthcheckMetrics::new(statsd_client);

    // Allow NODE_URL (exported from BBHC_SIDECAR_GETH_RPC in ConfigMap) to override.
    let effective_url =
        std::env::var("NODE_URL").unwrap_or_else(|_| config.node_url.clone());
    let node = Node::new(effective_url.clone(), config.new_instance);
    let client = AlloyEthClient::new_http(&effective_url)?;

    let mut checker: BlockProductionHealthChecker<_> =
        BlockProductionHealthChecker::new(node, client, config.healthcheck, metrics);

    // Spawn decoupled status emitter at 2s cadence.
    let _status_handle = checker.spawn_status_emitter(2000);

    // Basic run path: poll until Ctrl+C.
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = shutdown_tx.send(());
    });

    tokio::select! {
        () = checker.poll_for_health_checks() => {},
        _ = &mut shutdown_rx => {
            info!("shutdown signal received, exiting");
        }
    }

    Ok(())
}
