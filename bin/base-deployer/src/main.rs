#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod cli;
mod config;
mod devnet;
mod deploy;
mod external;
mod genesis;
mod runtime;

use clap::Parser;
use eyre::{ContextCompat, Result, WrapErr};

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
    let config = apply_cli_overrides(load_config(cli.config.as_deref())?, &cli);

    match cli.command {
        Commands::Genesis => {
            let resolved = config.resolve(cli.output_dir)?;
            let artifacts = genesis::generate_genesis(&resolved)
                .wrap_err("Failed to generate devnet genesis artifacts")?;
            println!("Generated devnet genesis artifacts in {}", artifacts.output_dir.display());
            Ok(())
        }
        Commands::DeployL1 { l1_rpc } => {
            let l1_rpc = l1_rpc.context("`base-deployer deploy-l1` requires --l1-rpc")?;
            let output = deploy::deploy_l1(config, cli.output_dir, &l1_rpc)
                .await
                .wrap_err("Failed to deploy L1 contracts")?;
            println!(
                "Deployed L1 contracts. Manifest: {}",
                output.manifest_path.display()
            );
            Ok(())
        }
        Commands::DeployL2 { l1_rpc } => {
            let output = deploy::deploy_l2(config, cli.output_dir, l1_rpc.as_deref())
                .await
                .wrap_err("Failed to extract L2 deployment artifacts")?;
            println!(
                "Generated L2 deployment artifacts. Genesis: {}",
                output.genesis_path.display()
            );
            Ok(())
        }
        Commands::Devnet { l1_rpc } => {
            match runtime::start_devnet(config, l1_rpc.as_deref())
                .await
                .wrap_err("Failed to start devnet")?
            {
                runtime::DevnetStartResult::Local(report) => {
                    println!("{}", runtime::format_local_report(&report));
                }
                runtime::DevnetStartResult::External(report) => {
                    println!(
                        "Prepared external-L1 deployment artifacts.\nManifest: {}\nGenesis: {}\nRollup: {}",
                        report.manifest_path.display(),
                        report.genesis_path.display(),
                        report.rollup_path.display(),
                    );
                }
            }
            Ok(())
        }
        Commands::Status { json } => {
            let status = runtime::collect_status()
                .await
                .wrap_err("Failed to inspect devnet status")?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&status)
                        .wrap_err("Failed to serialize status report")?
                );
            } else {
                println!("{}", runtime::format_local_report(&status));
            }
            Ok(())
        }
    }
}

fn load_config(path: Option<&std::path::Path>) -> Result<config::DeployerConfig> {
    path.map(config::DeployerConfig::load).transpose().map(Option::unwrap_or_default)
}

fn apply_cli_overrides(mut config: config::DeployerConfig, cli: &Cli) -> config::DeployerConfig {
    if let Some(l1_chain_id) = cli.l1_chain_id {
        config.l1_chain_id = Some(l1_chain_id);
    }
    if let Some(l2_chain_id) = cli.l2_chain_id {
        config.l2_chain_id = Some(l2_chain_id);
    }
    if let Some(slot_duration) = cli.slot_duration {
        config.slot_duration = Some(slot_duration);
    }
    if let Some(genesis_time) = cli.genesis_time {
        config.genesis_time = Some(genesis_time);
    }
    if let Some(ref prefund_balance) = cli.prefund_balance {
        config.prefund_balance = Some(prefund_balance.clone());
    }
    if let Some(l2_base_v1_block) = cli.l2_base_v1_block {
        config.l2_base_v1_block = Some(l2_base_v1_block);
    }
    if let Some(ref output_dir) = cli.output_dir {
        config.output_dir = Some(output_dir.clone());
    }

    config
}
