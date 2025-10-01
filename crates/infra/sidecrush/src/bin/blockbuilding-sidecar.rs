use clap::Parser;
use sidecrush::blockbuilding_healthcheck::{
    alloy_client::AlloyEthClient, BlockProductionHealthChecker, HealthcheckConfig, Node,
};
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
    #[arg(long, env, default_value = "text")]
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
        let _ = tracing_subscriber::fmt()
            .json()
            .with_max_level(args.log_level)
            .try_init();
    } else {
        let _ = tracing_subscriber::fmt()
            .with_max_level(args.log_level)
            .try_init();
    }

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
        BlockProductionHealthChecker::new(node, client, config);

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
