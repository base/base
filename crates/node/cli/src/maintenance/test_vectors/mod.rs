//! Command for generating test vectors.

use clap::{Parser, Subcommand};

mod compact;
pub use compact::{
    GENERATE_VECTORS, IDENTIFIER_TYPE, READ_VECTORS, VECTOR_SIZE, VECTORS_FOLDER, generate_vector,
    generate_vectors, generate_vectors_with, read_vector, read_vectors, read_vectors_with,
    type_name,
};
mod tables;
pub use tables::generate_vectors as generate_table_vectors;

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
    /// Randomly generate test vectors for each `Compact` type using the `--write` flag.
    ///
    /// The generated vectors are serialized in both `json` and `Compact` formats and saved to a
    /// file.
    ///
    /// Use the `--read` flag to read and validate the previously generated vectors from a file.
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
                tables::generate_vectors(names)?;
            }
            Subcommands::Compact { write, .. } => {
                if write {
                    compact::generate_vectors()?;
                } else {
                    compact::read_vectors()?;
                }
            }
        }
        Ok(())
    }
}
