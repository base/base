//! Global CLI arguments for the Base Reth node.
//!
//! This module provides common command-line arguments that are shared across
//! different CLI commands and subcommands.
//!
//! # Example
//!
//! ```ignore
//! use base_cli_utils::GlobalArgs;
//! use clap::Parser;
//!
//! #[derive(Debug, Parser)]
//! struct MyCli {
//!     #[command(flatten)]
//!     global: GlobalArgs,
//! }
//! ```

use super::LoggingArgs;

/// Global arguments shared across all CLI commands.
///
/// These arguments provide common configuration options that apply to the entire
/// application, such as network selection and logging configuration.
///
/// # Network Configuration
///
/// The chain ID can be configured via the `--chain-id` flag or the `BASE_NETWORK`
/// environment variable. If neither is specified, it defaults to Base Mainnet (8453).
///
/// # Example
///
/// ```ignore
/// // Using default Base Mainnet
/// $ my-cli
///
/// // Using Base Sepolia testnet
/// $ my-cli --chain-id 84532
///
/// // Using environment variable
/// $ BASE_NETWORK=84532 my-cli
/// ```
#[derive(Debug, Clone, clap::Args)]
pub struct GlobalArgs {
    /// The chain ID of the network to connect to.
    ///
    /// Defaults to Base Mainnet (8453). Can also be set via the `BASE_NETWORK`
    /// environment variable.
    ///
    /// Common values:
    /// - 8453: Base Mainnet
    /// - 84532: Base Sepolia
    #[arg(long = "chain-id", env = "BASE_NETWORK", default_value = "8453")]
    pub chain_id: u64,

    /// Logging configuration arguments.
    #[command(flatten)]
    pub logging: LoggingArgs,
}
