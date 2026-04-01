#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod cli;

use clap::Parser;
use eyre::{Result, bail};

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

    match cli.command {
        Commands::Genesis => bail!("`base-deployer genesis` is not implemented yet"),
        Commands::DeployL1 { .. } => bail!("`base-deployer deploy-l1` is not implemented yet"),
        Commands::DeployL2 { .. } => bail!("`base-deployer deploy-l2` is not implemented yet"),
        Commands::Devnet { .. } => bail!("`base-deployer devnet` is not implemented yet"),
        Commands::Status { .. } => bail!("`base-deployer status` is not implemented yet"),
    }
}
