//! A wrapper around the CLI.

use eyre::Result;
use reth_optimism_cli::chainspec::OpChainSpecParser;

use crate::{BuilderArgs, BuilderLauncher};

/// `RethCli` type alias for the OP builder.
pub type RethCli = reth_optimism_cli::Cli<OpChainSpecParser, BuilderArgs>;

/// Wraps the [`RethCli`] to provide a launcher.
#[derive(Debug)]
pub struct Cli {
    /// The inner reth CLI type.
    pub inner: RethCli,
}

impl Cli {
    /// Constructs a new instance of [`Cli`] by wrapping the provided [`RethCli`].
    pub const fn new(inner: RethCli) -> Self {
        Self { inner }
    }

    /// Launches the CLI application using the
    pub fn launch(self) -> Result<()> {
        let telemetry_args = match &self.inner.command {
            reth_optimism_cli::commands::Commands::Node(node_command) => {
                node_command.ext.telemetry.clone()
            }
            _ => Default::default(),
        };

        let mut cli_app = self.inner.configure();

        // Only setup telemetry if an OTLP endpoint is provided
        if telemetry_args.otlp_endpoint.is_some() {
            let telemetry_layer = telemetry_args.setup()?;
            cli_app.access_tracing_layers()?.add_layer(telemetry_layer);
        }

        tracing::info!("Starting OP builder in flashblocks mode");
        let launcher = BuilderLauncher::new();
        cli_app.run(launcher)?;
        Ok(())
    }
}
