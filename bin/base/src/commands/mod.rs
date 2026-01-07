//! Contains cli commands.

use clap::Subcommand;

pub mod consensus;
pub use consensus::ConsensusCommand;

pub mod execution;
pub use execution::ExecutionCommand;

pub mod mempool;
pub use mempool::MempoolCommand;

/// Subcommands for the CLI.
#[derive(Debug, Clone, Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum Commands {
    /// Runs the consensus layer for the Base Stack.
    #[command(alias = "c")]
    Consensus(ConsensusCommand),
    /// Runs the execution layer for the Base Stack.
    #[command(alias = "e")]
    Execution(ExecutionCommand),
    /// Streams pending transactions from the mempool.
    #[command(alias = "m")]
    Mempool(MempoolCommand),
}
