//! Global CLI arguments for the Base Reth node.

use super::LoggingArgs;

/// Global arguments shared across all CLI commands.
///
/// Chain ID defaults to Base Mainnet (8453). Can be set via `--chain-id` or `BASE_NETWORK` env.
#[derive(Debug, Clone, clap::Args)]
pub struct GlobalArgs {
    /// Chain ID (8453 = Base Mainnet, 84532 = Base Sepolia).
    #[arg(long = "chain-id", env = "BASE_NETWORK", default_value = "8453")]
    pub chain_id: u64,

    /// Logging configuration.
    #[command(flatten)]
    pub logging: LoggingArgs,
}
