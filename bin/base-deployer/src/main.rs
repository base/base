#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod cli;
mod config;
mod devnet;
mod genesis;

use clap::Parser;
use eyre::{Result, WrapErr, bail};

use self::cli::{Cli, Commands};

#[tokio::main]
async fn main() {
    base_cli_utils::init_common!();

    if let Err(err) = run().await {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = load_config(cli.config.as_deref())?;

    match cli.command {
        Commands::Genesis => {
            let resolved = config.resolve(cli.output_dir)?;
            let artifacts = genesis::generate_genesis(&resolved)
                .wrap_err("Failed to generate devnet genesis artifacts")?;
            println!("Generated devnet genesis artifacts in {}", artifacts.output_dir.display());
            Ok(())
        }
        Commands::DeployL1 { .. } => bail!("`base-deployer deploy-l1` is not implemented yet"),
        Commands::DeployL2 { .. } => bail!("`base-deployer deploy-l2` is not implemented yet"),
        Commands::Devnet { .. } => bail!("`base-deployer devnet` is not implemented yet"),
        Commands::Status { .. } => bail!("`base-deployer status` is not implemented yet"),
    }
}

fn load_config(path: Option<&std::path::Path>) -> Result<config::DeployerConfig> {
    path.map(config::DeployerConfig::load).transpose().map(Option::unwrap_or_default)
}
