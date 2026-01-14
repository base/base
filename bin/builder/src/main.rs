#![doc = "OP-Rbuilder Binary"]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use eyre::Result;
use op_rbuilder::{
    args::CliExt,
    builders::{BuilderMode, FlashblocksBuilder, StandardBuilder},
    launcher::BuilderLauncher,
};

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() -> Result<()> {
    // Parse CLI arguments with builder-specific configuration
    let cli = op_rbuilder::args::Cli::parsed();
    let mode = cli.builder_mode();

    // Extract telemetry args before configuring CLI (if telemetry feature enabled)
    #[cfg(feature = "telemetry")]
    let telemetry_args = match &cli.command {
        reth_optimism_cli::commands::Commands::Node(node_command) => {
            node_command.ext.telemetry.clone()
        }
        _ => Default::default(),
    };

    // Configure CLI application
    #[cfg(not(feature = "telemetry"))]
    let cli_app = cli.configure();

    #[cfg(feature = "telemetry")]
    let mut cli_app = cli.configure();
    #[cfg(feature = "telemetry")]
    {
        use op_rbuilder::primitives::telemetry::setup_telemetry_layer;
        let telemetry_layer = setup_telemetry_layer(&telemetry_args)?;
        cli_app.access_tracing_layers()?.add_layer(telemetry_layer);
    }

    // Launch the builder with the appropriate mode
    match mode {
        BuilderMode::Standard => {
            tracing::info!("Starting OP builder in standard mode");
            let launcher = BuilderLauncher::<StandardBuilder>::new();
            cli_app.run(launcher)?;
        }
        BuilderMode::Flashblocks => {
            tracing::info!("Starting OP builder in flashblocks mode");
            let launcher = BuilderLauncher::<FlashblocksBuilder>::new();
            cli_app.run(launcher)?;
        }
    }

    Ok(())
}
