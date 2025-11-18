use alloy_eips::BlockId;
use anyhow::Result;
use op_succinct_host_utils::{
    fetcher::{OPSuccinctDataFetcher, RPCMode},
    host::OPSuccinctHost,
    setup_logger, OP_SUCCINCT_L2_OUTPUT_ORACLE_CONFIG_PATH,
};
use op_succinct_proof_utils::initialize_host;
use op_succinct_scripts::config_common::{
    find_project_root, get_address, get_shared_config_data, write_config_file, TWO_WEEKS_IN_SECONDS,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{env, sync::Arc};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// The config for deploying the OPSuccinctL2OutputOracle.
/// Note: The fields should be in alphabetical order for Solidity to parse it correctly.
struct L2OOConfig {
    aggregation_vkey: String,
    challenger: String,
    fallback_timeout_secs: u64,
    finalization_period: u64,
    l2_block_time: u64,
    op_succinct_l2_output_oracle_impl: String,
    owner: String,
    proposer: String,
    proxy_admin: String,
    range_vkey_commitment: String,
    rollup_config_hash: String,
    starting_block_number: u64,
    starting_output_root: String,
    starting_timestamp: u64,
    submission_interval: u64,
    verifier: String,
}

/// Update the L2OO config with the rollup config hash and other relevant data before the contract
/// is deployed.
///
/// Specifically, updates the following fields in `opsuccinctl2ooconfig.json`:
/// - rollup_config_hash: Get the hash of the rollup config from the rollup config file.
/// - l2_block_time: Get the block time from the rollup config.
/// - starting_block_number: If `STARTING_BLOCK_NUMBER` is not set, set starting_block_number to the
///   latest finalized block on L2.
/// - starting_output_root: Set to the output root of the starting block number.
/// - starting_timestamp: Set to the timestamp of the starting block number.
/// - chain_id: Get the chain id from the rollup config.
/// - vkey: Get the vkey from the aggregation program ELF.
/// - owner: Set to the address associated with the private key.
async fn update_l2oo_config() -> Result<()> {
    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;
    let host = initialize_host(Arc::new(data_fetcher.clone()));
    let shared_config = get_shared_config_data(data_fetcher.clone()).await?;

    let rollup_config = data_fetcher.rollup_config.as_ref().unwrap();
    let l2_block_time = rollup_config.block_time;

    let submission_interval =
        env::var("SUBMISSION_INTERVAL").map(|p| p.parse().unwrap()).unwrap_or(10);

    // Default finalization period of 1 hour. Gives the challenger enough time to dispute the
    // output. Docs: https://docs.optimism.io/builders/chain-operators/configuration/rollup#finalizationperiodseconds
    const DEFAULT_FINALIZATION_PERIOD_SECS: u64 = 60 * 60;
    let finalization_period = env::var("FINALIZATION_PERIOD_SECS")
        .map(|p| p.parse().unwrap())
        .unwrap_or(DEFAULT_FINALIZATION_PERIOD_SECS);

    // Default to the address associated with the private key if the environment variable is not
    // set. If private key is not set, default to zero address.
    let proposer = get_address("PROPOSER", true);
    let owner = get_address("OWNER", true);
    let challenger = get_address("CHALLENGER", true);

    let proxy_admin = get_address("PROXY_ADMIN", false);
    let op_succinct_l2_output_oracle_impl = get_address("OP_SUCCINCT_L2_OUTPUT_ORACLE_IMPL", false);

    let fallback_timeout_secs = env::var("FALLBACK_TIMEOUT_SECS")
        .map(|p| p.parse().unwrap())
        .unwrap_or(TWO_WEEKS_IN_SECONDS);

    // Get starting block number - use latest finalized if not set.
    let starting_block_number = match env::var("STARTING_BLOCK_NUMBER") {
        Ok(n) => n.parse().unwrap(),
        Err(_) => {
            // Use finalized block minus the finalization period as a starting point
            let finalized_l2_header = data_fetcher.get_l2_header(BlockId::finalized()).await?;
            let finalized_l2_block = finalized_l2_header.number;

            let num_blocks_for_finality = finalization_period / l2_block_time;
            let search_start = finalized_l2_block.saturating_sub(num_blocks_for_finality);

            // Now search for the highest finalized block with available data
            let finalized_l2_block_number =
                match host.get_finalized_l2_block_number(&data_fetcher, search_start).await? {
                    Some(block_num) => block_num,
                    None => search_start,
                };

            finalized_l2_block_number.saturating_sub(num_blocks_for_finality)
        }
    };

    let starting_block_number_hex = format!("0x{starting_block_number:x}");
    let optimism_output_data: Value = data_fetcher
        .fetch_rpc_data_with_mode(
            RPCMode::L2Node,
            "optimism_outputAtBlock",
            vec![starting_block_number_hex.into()],
        )
        .await?;

    let starting_output_root = optimism_output_data["outputRoot"].as_str().unwrap().to_string();
    let starting_timestamp = optimism_output_data["blockRef"]["timestamp"].as_u64().unwrap();

    let l2oo_config = L2OOConfig {
        challenger,
        fallback_timeout_secs,
        finalization_period,
        l2_block_time,
        owner,
        proposer,
        rollup_config_hash: shared_config.rollup_config_hash,
        starting_block_number,
        starting_output_root,
        starting_timestamp,
        submission_interval,
        verifier: shared_config.verifier_address,
        aggregation_vkey: shared_config.aggregation_vkey,
        range_vkey_commitment: shared_config.range_vkey_commitment,
        proxy_admin,
        op_succinct_l2_output_oracle_impl,
    };

    write_config_file(&l2oo_config, &OP_SUCCINCT_L2_OUTPUT_ORACLE_CONFIG_PATH, "L2 Output Oracle")?;

    Ok(())
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
    setup_logger();

    let args = Args::parse();

    // This fetches the .env file from the project root. If the command is invoked in the contracts/
    // directory, the .env file in the root of the repo is used.
    if let Some(root) = find_project_root() {
        dotenv::from_path(root.join(&args.env_file)).ok();
        log::info!("Loaded {} from project root", args.env_file);
    } else {
        // Try to load the env file in case it's present
        if dotenv::from_path(args.env_file.clone()).is_ok() {
            log::info!("Loaded {} from current directory", args.env_file);
        } else {
            log::error!("Could not find env file. {} file not loaded", args.env_file);
        }
    }

    update_l2oo_config().await?;

    Ok(())
}
