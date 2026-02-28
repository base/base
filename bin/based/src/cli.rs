//! CLI definition for the based binary.

use based::{BasedConfig, HealthcheckConfig};
use base_cli_utils::LogConfig;
use clap::Parser;

base_cli_utils::define_log_args!("BBHC_SIDECAR");

/// CLI entry point for the block-building sidecar healthcheck service.
#[derive(Parser, Debug)]
#[command(author, version, about = "Blockbuilding sidecar healthcheck service")]
pub(crate) struct Args {
    /// Ethereum node HTTP RPC URL.
    #[arg(long, env, default_value = "http://localhost:8545")]
    node_url: String,

    /// Poll interval in milliseconds.
    #[arg(long, env = "BBHC_SIDECAR_POLL_INTERVAL_MS", default_value_t = 1000u64)]
    poll_interval_ms: u64,

    /// Grace period in milliseconds before considering delayed.
    #[arg(long, env = "BBHC_SIDECAR_GRACE_PERIOD_MS", default_value_t = 2000u64)]
    grace_period_ms: u64,

    /// Threshold in milliseconds to consider unhealthy/stalled.
    #[arg(long, env = "BBHC_SIDECAR_UNHEALTHY_NODE_THRESHOLD_MS", default_value_t = 3000u64)]
    unhealthy_node_threshold_ms: u64,

    /// Logging configuration.
    #[command(flatten)]
    log: LogArgs,

    /// Treat node as a new instance on startup (suppresses initial errors until healthy).
    #[arg(long, env, default_value_t = true)]
    new_instance: bool,
}

impl From<Args> for BasedConfig {
    fn from(args: Args) -> Self {
        Self {
            node_url: args.node_url,
            new_instance: args.new_instance,
            healthcheck: HealthcheckConfig::new(
                args.poll_interval_ms,
                args.grace_period_ms,
                args.unhealthy_node_threshold_ms,
            ),
            log: LogConfig::from(args.log),
        }
    }
}
