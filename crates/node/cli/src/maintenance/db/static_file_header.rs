use std::path::PathBuf;

use base_execution_state_maintenance::DbTool;
use base_execution_state_provider::StaticFileProviderFactory;
use base_execution_state_types::StaticFileSegment;
use clap::{Parser, Subcommand};
use tracing::warn;

/// The arguments for the `reth db static-file-header` command
#[derive(Parser, Debug)]
pub struct Command {
    #[command(subcommand)]
    source: Source,
}

/// Source for locating the static file
#[derive(Subcommand, Debug)]
enum Source {
    /// Query by segment and block number
    Block {
        /// Static file segment
        #[arg(value_enum)]
        segment: StaticFileSegment,
        /// Block number to query
        block: u64,
    },
    /// Query by path to static file
    Path {
        /// Path to the static file
        path: PathBuf,
    },
}

impl Command {
    /// Execute `db static-file-header` command
    pub fn execute(self, tool: &DbTool) -> eyre::Result<()> {
        let static_file_provider = tool.provider_factory.static_file_provider();
        if let Err(err) = static_file_provider.check_consistency(&tool.provider_factory.provider()?)
        {
            warn!("Error checking consistency of static files: {err}");
        }

        // Get the provider based on the source
        let provider = match self.source {
            Source::Path { path } => {
                static_file_provider.get_segment_provider_for_path(&path)?.ok_or_else(|| {
                    eyre::eyre!("Could not find static file segment for path: {}", path.display())
                })?
            }
            Source::Block { segment, block } => {
                static_file_provider.get_segment_provider(segment, block)?
            }
        };

        let header = provider.user_header();

        println!("Segment: {}", header.segment());
        println!("Expected Block Range: {}", header.expected_block_range());
        println!("Block Range: {:?}", header.block_range());
        println!("Transaction Range: {:?}", header.tx_range());

        Ok(())
    }
}
