use crate::builders::BuilderMode;
use clap::Parser;
pub use op::OpRbuilderArgs;
use playground::PlaygroundOptions;
use reth_optimism_cli::{chainspec::OpChainSpecParser, commands::Commands};

mod op;
mod playground;

/// This trait is used to extend Reth's CLI with additional functionality that
/// are specific to the OP builder, such as populating default values for CLI arguments
/// when running in the playground mode or checking the builder mode.
///
pub trait CliExt {
    /// Populates the default values for the CLI arguments when the user specifies
    /// the `--builder.playground` flag.
    fn populate_defaults(self) -> Self;

    /// Returns the builder mode that the node is started with.
    fn builder_mode(&self) -> BuilderMode;

    /// Returns the Cli instance with the parsed command line arguments
    /// and defaults populated if applicable.
    fn parsed() -> Self;
}

pub type Cli = reth_optimism_cli::Cli<OpChainSpecParser, OpRbuilderArgs>;

impl CliExt for Cli {
    /// Checks if the node is started with the `--builder.playground` flag,
    /// and if so, populates the default values for the CLI arguments from the
    /// playground configuration.
    ///
    /// The `--builder.playground` flag is used to populate the CLI arguments with
    /// default values for running the builder against the playground environment.
    ///
    /// The values are populated from the default directory of the playground
    /// configuration, which is `$HOME/.playground/devnet/` by default.
    ///
    /// Any manually specified CLI arguments by the user will override the defaults.
    fn populate_defaults(self) -> Self {
        let Commands::Node(ref node_command) = self.command else {
            // playground defaults are only relevant if running the node commands.
            return self;
        };

        let Some(ref playground_dir) = node_command.ext.playground else {
            // not running in playground mode.
            return self;
        };

        let options = match PlaygroundOptions::new(playground_dir) {
            Ok(options) => options,
            Err(e) => exit(e),
        };

        options.apply(self)
    }

    fn parsed() -> Self {
        Cli::parse().populate_defaults()
    }

    /// Returns the type of builder implementation that the node is started with.
    /// Currently supports `Standard` and `Flashblocks` modes.
    fn builder_mode(&self) -> BuilderMode {
        if let Commands::Node(ref node_command) = self.command {
            if node_command.ext.enable_flashblocks {
                return BuilderMode::Flashblocks;
            }
        }
        BuilderMode::Standard
    }
}

/// Following clap's convention, a failure to parse the command line arguments
/// will result in terminating the program with a non-zero exit code.
fn exit(error: eyre::Report) -> ! {
    eprintln!("{error}");
    std::process::exit(-1);
}
