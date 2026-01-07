//! Contains the CLI entry point for the Base binary.

use anyhow::Result;
use clap::{builder::styling::Styles, Parser};

use crate::commands::Commands;
use crate::flags::GlobalArgs;
use crate::version;

/// Returns the CLI styles.
const fn cli_styles() -> Styles {
    Styles::plain()
}

/// The CLI.
#[derive(Parser, Clone, Debug)]
#[command(
    author,
    version = version::SHORT_VERSION,
    long_version = version::LONG_VERSION,
    about,
    styles = cli_styles(),
    long_about = None
)]
pub struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    pub subcommand: Commands,
    /// Global arguments for the CLI.
    #[command(flatten)]
    pub global: GlobalArgs,
}

impl Cli {
    /// Runs the CLI.
    pub fn run(self) -> Result<()> {
        // TODO: Initialize telemetry - allow subcommands to customize the filter.

        // TODO: Initialize unified metrics

        // TODO: Allow subcommands to initialize cli metrics.

        // Run the subcommand.
        match self.subcommand {
            Commands::Consensus(c) => Self::run_until_ctrl_c(c.run(&self.global)),
            Commands::Execution(e) => Self::run_until_ctrl_c(e.run(&self.global)),
            Commands::Mempool(m) => Self::run_until_ctrl_c(m.run(&self.global)),
        }
    }

    /// Run until ctrl-c is pressed.
    pub fn run_until_ctrl_c<F>(fut: F) -> Result<()>
    where
        F: std::future::Future<Output = Result<()>>,
    {
        let rt = Self::tokio_runtime().map_err(|e| anyhow::anyhow!(e))?;
        rt.block_on(async move {
            tokio::select! {
                res = fut => res,
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!(target: "cli", "Received Ctrl-C, shutting down...");
                    Ok(())
                }
            }
        })
    }

    /// Creates a new default tokio multi-thread [Runtime](tokio::runtime::Runtime) with all
    /// features enabled
    pub fn tokio_runtime() -> Result<tokio::runtime::Runtime, std::io::Error> {
        tokio::runtime::Builder::new_multi_thread().enable_all().build()
    }
}
