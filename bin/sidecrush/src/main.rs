//! Block-production health-check sidecar binary. Long-lived process that polls an
//! execution-layer HTTP RPC endpoint and emits four `StatsD` counters (`base.blocks.healthy`,
//! `base.blocks.delayed`, `base.blocks.unhealthy`, `base.blocks.error`) to the local Datadog
//! agent. All meaningful logic lives in the `base-sidecrush` library crate.

use std::net::UdpSocket;

use base_sidecrush::{
    AlloyEthClient, BlockProductionHealthChecker, HealthcheckConfig, HealthcheckMetrics, Node,
};
use cadence::{StatsdClient, UdpMetricSink};
use clap::{ArgAction, Parser};
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
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

    /// Treat node as a new instance on startup (suppresses initial errors until healthy).
    /// Accepts `--new-instance=true|false` so it can be disabled explicitly.
    #[arg(long, env, default_value_t = true, action = ArgAction::Set)]
    new_instance: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if args.log_format.to_lowercase() == "json" {
        let _ = tracing_subscriber::fmt().json().with_max_level(args.log_level).try_init();
    } else {
        let _ = tracing_subscriber::fmt().with_max_level(args.log_level).try_init();
    }

    let statsd_host = std::env::var("DD_AGENT_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let statsd_addr = format!("{statsd_host}:8125");
    tracing::info!(address = %statsd_addr, "connecting to StatsD agent");

    let socket = UdpSocket::bind("0.0.0.0:0").expect("failed to bind UDP socket");
    socket.set_nonblocking(true).expect("failed to set socket nonblocking");
    let sink =
        UdpMetricSink::from(statsd_addr.as_str(), socket).expect("failed to create StatsD sink");

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
        "initialized StatsD client with tags"
    );

    let metrics = HealthcheckMetrics::new(statsd_client);

    let node = Node::new(args.node_url.clone(), args.new_instance);
    let client = AlloyEthClient::new_http(&args.node_url).expect("failed to create client");
    let config = HealthcheckConfig::new(
        args.poll_interval_ms,
        args.grace_period_ms,
        args.unhealthy_node_threshold_ms,
    );

    let mut checker: BlockProductionHealthChecker<_> =
        BlockProductionHealthChecker::new(node, client, config, metrics);

    let _status_handle = checker.spawn_status_emitter(2000);

    tokio::select! {
        _ = checker.poll_for_health_checks() => {},
        received = shutdown_signal() => {
            tracing::info!(signal = received, "shutdown signal received, exiting");
        }
    }
}

/// Wait for a graceful-shutdown signal.
///
/// On Unix this races `SIGTERM` (the default signal Kubernetes sends on pod shutdown) and
/// `SIGINT` (Ctrl-C) so the container exits promptly instead of waiting for the `SIGKILL`
/// that follows `terminationGracePeriodSeconds`. On non-Unix targets it falls back to
/// Ctrl-C only.
#[cfg(unix)]
async fn shutdown_signal() -> &'static str {
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
