use alloy::{eips::BlockId, hex, signers::local::PrivateKeySigner};
use alloy_primitives::{Address, B256};
use anyhow::Result;
use op_succinct_client_utils::{boot::hash_rollup_config, types::u32_to_u8};
use op_succinct_host_utils::fetcher::{OPSuccinctDataFetcher, RPCMode, RunContext};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sp1_sdk::{HashableKey, Prover, ProverClient};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

pub const AGG_ELF: &[u8] = include_bytes!("../../../elf/aggregation-elf");
pub const RANGE_ELF: &[u8] = include_bytes!("../../../elf/range-elf");

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// The config for deploying the OPSuccinctL2OutputOracle.
/// Note: The fields should be in alphabetical order for Solidity to parse it correctly.
struct L2OOConfig {
    challenger: String,
    finalization_period: u64,
    l2_block_time: u64,
    owner: String,
    proposer: String,
    rollup_config_hash: String,
    starting_block_number: u64,
    starting_output_root: String,
    starting_timestamp: u64,
    submission_interval: u64,
    verifier: String,
    aggregation_vkey: String,
    range_vkey_commitment: String,
}

/// If the environment variable is set for the address, return it. Otherwise, return the address associated with the private key. If the private key is not set, return the zero address.
fn get_address(env_var: &str) -> String {
    let private_key = env::var("PRIVATE_KEY").unwrap_or_else(|_| B256::ZERO.to_string());

    env::var(env_var).unwrap_or_else(|_| {
        if private_key == B256::ZERO.to_string() {
            Address::ZERO.to_string()
        } else {
            let signer: PrivateKeySigner = private_key.parse().unwrap();
            signer.address().to_string()
        }
    })
}

/// Update the L2OO config with the rollup config hash and other relevant data before the contract is deployed.
///
/// Specifically, updates the following fields in `opsuccinctl2ooconfig.json`:
/// - rollup_config_hash: Get the hash of the rollup config from the rollup config file.
/// - l2_block_time: Get the block time from the rollup config.
/// - starting_block_number: If `STARTING_BLOCK_NUMBER` is not set, set starting_block_number to the latest finalized block on L2.
/// - starting_output_root: Set to the output root of the starting block number.
/// - starting_timestamp: Set to the timestamp of the starting block number.
/// - chain_id: Get the chain id from the rollup config.
/// - vkey: Get the vkey from the aggregation program ELF.
/// - owner: Set to the address associated with the private key.
async fn update_l2oo_config() -> Result<()> {
    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config(RunContext::Dev).await?;

    let workspace_root = cargo_metadata::MetadataCommand::new()
        .exec()?
        .workspace_root;

    // Set the verifier address
    let verifier = env::var("VERIFIER_ADDRESS").unwrap_or_else(|_| {
        // Default to Groth16 VerifierGateway contract address
        // Source: https://docs.succinct.xyz/docs/verification/onchain/contract-addresses
        "0x397A5f7f3dBd538f23DE225B51f532c34448dA9B".to_string()
    });

    let starting_block_number = match env::var("STARTING_BLOCK_NUMBER") {
        Ok(n) => n.parse().unwrap(),
        Err(_) => {
            data_fetcher
                .get_l2_header(BlockId::finalized())
                .await
                .unwrap()
                .number
        }
    };

    let starting_block_number_hex = format!("0x{:x}", starting_block_number);
    let optimism_output_data: Value = data_fetcher
        .fetch_rpc_data_with_mode(
            RPCMode::L2Node,
            "optimism_outputAtBlock",
            vec![starting_block_number_hex.into()],
        )
        .await?;

    let starting_output_root = optimism_output_data["outputRoot"]
        .as_str()
        .unwrap()
        .to_string();
    let starting_timestamp = optimism_output_data["blockRef"]["timestamp"]
        .as_u64()
        .unwrap();

    let rollup_config = data_fetcher.rollup_config.as_ref().unwrap();
    let rollup_config_hash = format!("0x{:x}", hash_rollup_config(rollup_config));
    let l2_block_time = rollup_config.block_time;

    let submission_interval = env::var("SUBMISSION_INTERVAL")
        .map(|p| p.parse().unwrap())
        .unwrap_or(1000);

    // Default finalization period of 1 hour. Gives the challenger enough time to dispute the output.
    // Docs: https://docs.optimism.io/builders/chain-operators/configuration/rollup#finalizationperiodseconds
    const DEFAULT_FINALIZATION_PERIOD_SECS: u64 = 60 * 60;
    let finalization_period = env::var("FINALIZATION_PERIOD_SECS")
        .map(|p| p.parse().unwrap())
        .unwrap_or(DEFAULT_FINALIZATION_PERIOD_SECS);

    // Default to the address associated with the private key if the environment variable is not set. If private key is not set, default to zero address.
    let proposer = get_address("PROPOSER");
    let owner = get_address("OWNER");
    let challenger = get_address("CHALLENGER");

    let prover = ProverClient::builder().cpu().build();
    let (_, agg_vkey) = prover.setup(AGG_ELF);
    let aggregation_vkey = agg_vkey.vk.bytes32();

    let (_, range_vkey) = prover.setup(RANGE_ELF);
    let range_vkey_commitment = format!("0x{}", hex::encode(u32_to_u8(range_vkey.vk.hash_u32())));

    let l2oo_config = L2OOConfig {
        challenger,
        finalization_period,
        l2_block_time,
        owner,
        proposer,
        rollup_config_hash,
        starting_block_number,
        starting_output_root,
        starting_timestamp,
        submission_interval,
        verifier,
        aggregation_vkey,
        range_vkey_commitment,
    };

    write_l2oo_config(l2oo_config, workspace_root.as_std_path())?;

    Ok(())
}

/// Write the L2OO rollup config to `contracts/opsuccinctl2ooconfig.json`.
fn write_l2oo_config(config: L2OOConfig, workspace_root: &Path) -> Result<()> {
    let opsuccinct_config_path = workspace_root.join("contracts/opsuccinctl2ooconfig.json");
    // Create parent directories if they don't exist
    if let Some(parent) = opsuccinct_config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Write the L2OO rollup config to the opsuccinctl2ooconfig.json file
    fs::write(
        &opsuccinct_config_path,
        serde_json::to_string_pretty(&config)?,
    )?;
    Ok(())
}

fn find_project_root() -> Option<PathBuf> {
    let mut path = std::env::current_dir().ok()?;
    while !path.join(".git").exists() {
        if !path.pop() {
            return None;
        }
    }
    Some(path)
}

use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// L2 chain ID
    #[arg(long, default_value = ".env")]
    env_file: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // This fetches the .env file from the project root. If the command is invoked in the contracts/ directory,
    // the .env file in the root of the repo is used.
    if let Some(root) = find_project_root() {
        dotenv::from_path(root.join(args.env_file)).ok();
    } else {
        eprintln!(
            "Warning: Could not find project root. {} file not loaded.",
            args.env_file
        );
    }

    update_l2oo_config().await?;

    Ok(())
}
