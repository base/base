use base_execution_state_database::static_file::iter_static_files;
use base_execution_state_database::{Database, DbTx, DbTxMut, Table, TableViewer, Tables};
use base_execution_state_types::StaticFileSegment;
use clap::{Parser, Subcommand};
use base_execution_state_maintenance::DbTool;
use base_execution_state_provider::StaticFileProviderFactory;

/// The arguments for the `reth db clear` command
#[derive(Parser, Debug)]
pub struct Command {
    #[command(subcommand)]
    subcommand: Subcommands,
}

impl Command {
    /// Execute `db clear` command
    pub fn execute(self, tool: &DbTool) -> eyre::Result<()> {
        match self.subcommand {
            Subcommands::Mdbx { table } => {
                table.view(&ClearViewer { db: tool.provider_factory.db_ref() })?
            }
            Subcommands::StaticFile { segment } => {
                let static_file_provider = tool.provider_factory.static_file_provider();
                let static_files = iter_static_files(static_file_provider.directory())?;

                if let Some(segment_static_files) = static_files.get(segment) {
                    for (block_range, _) in segment_static_files {
                        static_file_provider.delete_jar(segment, block_range.start())?;
                    }
                }
            }
        }

        Ok(())
    }
}

#[derive(Subcommand, Debug)]
enum Subcommands {
    /// Deletes all database table entries
    Mdbx { table: Tables },
    /// Deletes all static file segment entries
    StaticFile { segment: StaticFileSegment },
}

struct ClearViewer<'a, DB: Database> {
    db: &'a DB,
}

impl<DB: Database> TableViewer<()> for ClearViewer<'_, DB> {
    type Error = eyre::Report;

    fn view<T: Table>(&self) -> Result<(), Self::Error> {
        let tx = self.db.tx_mut()?;
        tx.clear::<T>()?;
        tx.commit()?;
        Ok(())
    }
}
