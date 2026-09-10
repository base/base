//! Command for generating test vectors.

use std::sync::Arc;

use base_common_chain_config::BaseChainSpec;
use base_common_types_chain::TxDeposit;
use clap::{Parser, Subcommand};

use crate::{
    GENERATE_VECTORS as ETH_GENERATE_VECTORS, READ_VECTORS as ETH_READ_VECTORS,
    generate_table_vectors, generate_vector, generate_vectors_with, read_vector, read_vectors_with,
};

/// Generate test-vectors for different data types.
#[derive(Debug, Parser)]
pub struct Command {
    #[command(subcommand)]
    command: Subcommands,
}

#[derive(Subcommand, Debug)]
/// `reth test-vectors` subcommands
pub enum Subcommands {
    /// Generates test vectors for specified tables. If no table is specified, generate for all.
    Tables {
        /// List of table names. Case-sensitive.
        names: Vec<String>,
    },
    /// Generates test vectors for `Compact` types with `--write`. Reads and checks generated
    /// vectors with `--read`.
    #[group(multiple = false, required = true)]
    Compact {
        /// Write test vectors to a file.
        #[arg(long)]
        write: bool,

        /// Read test vectors from a file.
        #[arg(long)]
        read: bool,
    },
}

impl Command {
    /// Execute the command
    pub async fn execute(self) -> eyre::Result<()> {
        match self.command {
            Subcommands::Tables { names } => {
                generate_table_vectors(names)?;
            }
            Subcommands::Compact { write, .. } => {
                if write {
                    generate_vectors_with(ETH_GENERATE_VECTORS)?;
                    generate_vectors_with(&[generate_vector::<TxDeposit>])?;
                } else {
                    read_vectors_with(ETH_READ_VECTORS)?;
                    read_vectors_with(&[read_vector::<TxDeposit>])?;
                }
            }
        }
        Ok(())
    }
    /// Returns the underlying chain being used to run this command
    pub const fn chain_spec(&self) -> Option<&Arc<BaseChainSpec>> {
        None
    }
}
