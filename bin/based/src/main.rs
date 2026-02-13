//! Based binary entry point.

use std::net::UdpSocket;

use based::{
    BlockProductionHealthChecker, HealthcheckConfig, HealthcheckMetrics, Node,
    alloy_client::AlloyEthClient,
};
use cadence::{StatsdClient, UdpMetricSink};
use clap::Parser;
use tracing::Level;

#[derive(Parser, Debug)]
#[command(author, version, about = "Blockbuilding sidecar healthcheck service")]
struct Args {
    /// Ethereum node HTTP RPC URL
    #[arg(long, env, default_value = "http://localhost:8545")]
    node_url: String,

    /// Poll interval in milliseconds
    #[arg(long, env = "BBHC_SIDECAR_POLL_INTERVAL_MS", default_value_t = 1000u64)]
    poll_interval_ms: u64,

    /// Grace period in milliseconds before considering delayed
    #[arg(long, env = "BBHC_SIDECAR_GRACE_PERIOD_MS", default_value_t = 2000u64)]
    grace_period_ms: u64,

    /// Threshold in milliseconds to consider unhealthy/stalled
    #[arg(long, env = "BBHC_SIDECAR_UNHEALTHY_NODE_THRESHOLD_MS", default_value_t = 3000u64)]
    unhealthy_node_threshold_ms: u64,

    /// Log level
    #[arg(long, env, default_value_t = Level::INFO)]
    log_level: Level,

    /// Log format (text|json)
    #[arg(long, env, default_value = "json")]
    log_format: String,

    /// Treat node as a new instance on startup (suppresses initial errors until healthy)
    #[arg(long, env, default_value_t = true)]
    new_instance: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Initialize logging
    if args.log_format.to_lowercase() == "json" {
        let _ = tracing_subscriber::fmt().json().with_max_level(args.log_level).try_init();
    } else {
        let _ = tracing_subscriber::fmt().with_max_level(args.log_level).try_init();
    }

    // Initialize StatsD client (sends to Datadog agent)
    // Use DD_AGENT_HOST if set (Kubernetes), otherwise localhost
    let statsd_host = std::env::var("DD_AGENT_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let statsd_addr = format!("{statsd_host}:8125");
    tracing::info!(address = %statsd_addr, "Connecting to StatsD agent");

    let socket = UdpSocket::bind("0.0.0.0:0").expect("failed to bind UDP socket");
    socket.set_nonblocking(true).expect("failed to set socket nonblocking");
    let sink =
        UdpMetricSink::from(statsd_addr.as_str(), socket).expect("failed to create StatsD sink");

    // Read tags from CODEFLOW environment variables
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

    tracing::info!(
        configname = %config_name,
        environment = %environment,
        projectname = %project_name,
        servicename = %service_name,
        "Initialized StatsD client with tags"
    );

    let metrics = HealthcheckMetrics::new(statsd_client);

    // Allow NODE_URL (exported from BBHC_SIDECAR_GETH_RPC in ConfigMap) to override
    let effective_url = std::env::var("NODE_URL").unwrap_or_else(|_| args.node_url.clone());
    let node = Node::new(effective_url.clone(), args.new_instance);
    let client = AlloyEthClient::new_http(&effective_url).expect("failed to create client");
    let config = HealthcheckConfig::new(
        args.poll_interval_ms,
        args.grace_period_ms,
        args.unhealthy_node_threshold_ms,
    );

    let mut checker: BlockProductionHealthChecker<_> =
        BlockProductionHealthChecker::new(node, client, config, metrics);

    // Spawn decoupled status emitter at 2s cadence
    let _status_handle = checker.spawn_status_emitter(2000);

    // Basic run path: poll until Ctrl+C
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = shutdown_tx.send(());
    });

    tokio::select! {
        _ = checker.poll_for_health_checks() => {},
        _ = &mut shutdown_rx => {
            tracing::info!(message = "Shutdown signal received, exiting");
        }
    }
}
