//! Block-production health-check sidecar binary. Long-lived process that polls an
//! execution-layer HTTP RPC endpoint and emits four `StatsD` counters (`base.blocks.healthy`,
//! `base.blocks.delayed`, `base.blocks.unhealthy`, `base.blocks.error`) to the local Datadog
//! agent. All meaningful logic lives in the `base-sidecrush` library crate.
//!
//! When `--ws-url` / `BBHC_SIDECAR_WS_URL` is set, block age is measured as the time since the
//! last `newHeads` notification instead of from polled header timestamps, which is required
//! for meaningful thresholds with sub-second block times.

use std::net::UdpSocket;

use base_sidecrush::{
    AlloyEthClient, AlloyWsHeadSubscriber, BlockProductionHealthChecker,
    HeadSubscriptionHealthChecker, HealthcheckConfig, HealthcheckMetrics, Node,
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

    /// Ethereum node WebSocket RPC URL. When set, health is driven by an `eth_subscribe`
    /// `newHeads` subscription and `--node-url` is not used.
    #[arg(long, env = "BBHC_SIDECAR_WS_URL")]
    ws_url: Option<String>,

    /// Poll interval in milliseconds. In WebSocket mode, how often block age is re-evaluated.
    /// Must be below the grace period, or the delayed state can never be observed.
    #[arg(
        long,
        env = "BBHC_SIDECAR_POLL_INTERVAL_MS",
        default_value_t = 1000u64,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    poll_interval_ms: u64,

    /// Grace period in milliseconds before considering delayed
    #[arg(long, env = "BBHC_SIDECAR_GRACE_PERIOD_MS", default_value_t = 2000u64)]
    grace_period_ms: u64,

    /// Threshold in milliseconds to consider unhealthy/stalled
    #[arg(long, env = "BBHC_SIDECAR_UNHEALTHY_NODE_THRESHOLD_MS", default_value_t = 3000u64)]
    unhealthy_node_threshold_ms: u64,

    /// Interval in milliseconds at which the current state is emitted to `StatsD`
    #[arg(
        long,
        env = "BBHC_SIDECAR_STATUS_INTERVAL_MS",
        default_value_t = 2000u64,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    status_interval_ms: u64,

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

    let config = HealthcheckConfig::new(
        args.poll_interval_ms,
        args.grace_period_ms,
        args.unhealthy_node_threshold_ms,
    );

    if let Some(ws_url) = args.ws_url {
        // In WebSocket mode the poll interval is how often the age of the last head is
        // re-evaluated. If it is not below the grace period, a stall jumps straight from
        // healthy to unhealthy and `delayed` can never fire.
        if args.poll_interval_ms >= args.grace_period_ms {
            tracing::error!(
                poll_interval_ms = args.poll_interval_ms,
                grace_period_ms = args.grace_period_ms,
                "poll_interval_ms must be below grace_period_ms in WebSocket mode, or the delayed state can never be observed"
            );
            return;
        }
        let node = Node::new(ws_url.clone(), args.new_instance);
        tracing::info!(sequencer = %node.display_url, "using newHeads subscription health check");
        let subscriber = AlloyWsHeadSubscriber::new(ws_url);
        let checker = HeadSubscriptionHealthChecker::new(node, subscriber, config, metrics);
        let _status_handle = checker.spawn_status_emitter(args.status_interval_ms);
        tokio::select! {
            _ = checker.run() => {},
            received = shutdown_signal() => {
                tracing::info!(signal = received, "shutdown signal received, exiting");
            }
        }
    } else {
        let node = Node::new(args.node_url.clone(), args.new_instance);
        tracing::info!(sequencer = %node.display_url, "using HTTP polling health check");
        let client = AlloyEthClient::new_http(&args.node_url).expect("failed to create client");
        let mut checker: BlockProductionHealthChecker<_> =
            BlockProductionHealthChecker::new(node, client, config, metrics);
        let _status_handle = checker.spawn_status_emitter(args.status_interval_ms);
        tokio::select! {
            _ = checker.poll_for_health_checks() => {},
            received = shutdown_signal() => {
                tracing::info!(signal = received, "shutdown signal received, exiting");
            }
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
