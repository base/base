//! CLI argument definitions for proposer.

use std::time::Duration;

use clap::Parser;

/// Proposer - TEE-based output proposal generation for OP Stack chains.
#[derive(Debug, Parser)]
#[command(name = "proposer")]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// Polling interval for new blocks (e.g., "12s", "1m").
    #[arg(
        long,
        env = "BASE_PROPOSER_POLL_INTERVAL",
        default_value = "12s",
        value_parser = parse_duration
    )]
    pub poll_interval: Duration,
}

/// Parse a duration string like "12s", "5m", "1h".
fn parse_duration(s: &str) -> Result<Duration, humantime::DurationError> {
    humantime::parse_duration(s)
}
