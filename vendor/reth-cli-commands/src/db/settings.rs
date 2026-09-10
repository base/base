//! `reth db settings` command for managing storage settings

use clap::{Parser, Subcommand};
use base_execution_state_maintenance::DbTool;
use base_execution_state_provider::MetadataProvider;

use crate::common::AccessRights;

/// `reth db settings` subcommand
#[derive(Debug, Parser)]
pub struct Command {
    #[command(subcommand)]
    command: Subcommands,
}

impl Command {
    /// Returns database access rights required for the command.
    pub fn access_rights(&self) -> AccessRights {
        match self.command {
            Subcommands::Get => AccessRights::RO,
        }
    }
}

#[derive(Debug, Clone, Copy, Subcommand)]
enum Subcommands {
    /// Get current storage settings from database
    Get,
}

impl Command {
    /// Execute the command
    pub fn execute(self, tool: &DbTool) -> eyre::Result<()> {
        match self.command {
            Subcommands::Get => self.get(tool),
        }
    }

    fn get(&self, tool: &DbTool) -> eyre::Result<()> {
        // Read storage settings
        let provider = tool.provider_factory.provider()?;
        let storage_settings = provider.storage_settings()?;

        // Display settings
        match storage_settings {
            Some(settings) => {
                println!("Current storage settings:");
                println!("{settings:#?}");
            }
            None => {
                println!("No storage settings found.");
            }
        }

        Ok(())
    }
}
