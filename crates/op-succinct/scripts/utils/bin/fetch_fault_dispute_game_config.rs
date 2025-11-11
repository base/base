use std::{env, sync::Arc};

use alloy_eips::BlockId;
use anyhow::Result;
use fault_proof::config::FaultDisputeGameConfig;
use op_succinct_host_utils::{
    fetcher::{OPSuccinctDataFetcher, RPCMode},
    host::OPSuccinctHost,
    OP_SUCCINCT_FAULT_DISPUTE_GAME_CONFIG_PATH,
};
use op_succinct_proof_utils::initialize_host;
use op_succinct_scripts::config_common::{
    find_project_root, get_shared_config_data, parse_addresses, write_config_file,
    TWO_WEEKS_IN_SECONDS,
};
use serde_json::Value;

/// Updates and generates the fault dispute game configuration file.
///
/// This function fetches the necessary configuration parameters from environment variables
/// and shared configuration data to generate a JSON configuration file used for deploying
/// the OPSuccinctFaultDisputeGame contract.
///
/// # Environment Variables
///
/// ## Game Configuration
/// - `GAME_TYPE`: Unique identifier for the dispute game type (default: "42")
///
/// ## Timing Configuration
/// - `DISPUTE_GAME_FINALITY_DELAY_SECONDS`: Delay in seconds before a dispute game can be finalized
///   (default: "604800" = 7 days)
/// - `MAX_CHALLENGE_DURATION`: Maximum duration in seconds for challenges (default: "604800" = 7
///   days)
/// - `MAX_PROVE_DURATION`: Maximum duration in seconds for proving (default: "86400" = 1 day)
/// - `FALLBACK_TIMEOUT_FP_SECS`: Timeout in seconds for permissionless proposing fallback (default:
///   1209600 = 2 weeks)
///
/// ## Bond Configuration
/// - `INITIAL_BOND_WEI`: Initial bond amount in wei required to create a dispute game (default:
///   "1000000000000000" = 0.001 ETH)
/// - `CHALLENGER_BOND_WEI`: Bond amount in wei required to challenge a game (default:
///   "1000000000000000" = 0.001 ETH)
///
/// ## Access Control Configuration
/// - `PERMISSIONLESS_MODE`: If "true", anyone can propose or challenge games; if "false", only
///   authorized addresses can (default: "false")
/// - `PROPOSER_ADDRESSES`: Comma-separated list of addresses authorized to propose games (ignored
///   if permissionless mode is true)
/// - `CHALLENGER_ADDRESSES`: Comma-separated list of addresses authorized to challenge games
///   (ignored if permissionless mode is true)
///
/// ## Contract Configuration
/// - `OPTIMISM_PORTAL2_ADDRESS`: Address of the OptimismPortal2 contract. If not provided or set to
///   zero address, a MockOptimismPortal2 will be deployed (default: zero address)
///
/// ## Starting State Configuration
/// - `STARTING_L2_BLOCK_NUMBER`: L2 block number to use as the starting point for the dispute game.
///   If not provided, it's calculated as: `latest_finalized_block -
///   (dispute_game_finality_delay_seconds / block_time)`
///
/// # Shared Configuration
///
/// The function also retrieves the following from shared configuration data:
/// - `aggregation_vkey`: Aggregation verification key
/// - `range_vkey_commitment`: Range verification key commitment
/// - `rollup_config_hash`: Hash of the rollup configuration
/// - `verifier_address`: Address of the SP1 verifier contract
/// - `use_sp1_mock_verifier`: Whether to use the mock verifier for testing
///
/// # Output
///
/// Generates `contracts/opsuccinctfdgconfig.json` containing all configuration parameters
/// needed for the Solidity deployment scripts.
async fn update_fdg_config() -> Result<()> {
    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;
    let host = initialize_host(Arc::new(data_fetcher.clone()));
    let shared_config = get_shared_config_data().await?;

    // Game configuration.
    let game_type = env::var("GAME_TYPE").unwrap_or("42".to_string()).parse().unwrap();

    // Timing configuration.
    let dispute_game_finality_delay_seconds = env::var("DISPUTE_GAME_FINALITY_DELAY_SECONDS")
        .unwrap_or("604800".to_string()) // 7 days default
        .parse()
        .unwrap();

    let max_challenge_duration = env::var("MAX_CHALLENGE_DURATION")
        .unwrap_or("604800".to_string()) // 7 days default
        .parse()
        .unwrap();

    let max_prove_duration = env::var("MAX_PROVE_DURATION")
        .unwrap_or("86400".to_string()) // 1 day default
        .parse()
        .unwrap();

    let fallback_timeout_fp_secs = env::var("FALLBACK_TIMEOUT_FP_SECS")
        .map(|p| p.parse().unwrap())
        .unwrap_or(TWO_WEEKS_IN_SECONDS);

    // Bond configuration.
    let initial_bond_wei = env::var("INITIAL_BOND_WEI")
        .unwrap_or("1000000000000000".to_string()) // 0.001 ETH default
        .parse()
        .unwrap();

    let challenger_bond_wei = env::var("CHALLENGER_BOND_WEI")
        .unwrap_or("1000000000000000".to_string()) // 0.001 ETH default
        .parse()
        .unwrap();

    // Access control configuration.
    let permissionless_mode =
        env::var("PERMISSIONLESS_MODE").unwrap_or("false".to_string()).parse().unwrap();

    let proposer_addresses =
        if permissionless_mode { vec![] } else { parse_addresses("PROPOSER_ADDRESSES") };

    let challenger_addresses =
        if permissionless_mode { vec![] } else { parse_addresses("CHALLENGER_ADDRESSES") };

    // OptimismPortal2 configuration.
    let optimism_portal2_address = env::var("OPTIMISM_PORTAL2_ADDRESS").unwrap_or_else(|_| {
        // Default to zero address if not provided - will deploy MockOptimismPortal2
        "0x0000000000000000000000000000000000000000".to_string()
    });

    // Get starting block number - use `latest finalized - dispute game finality delay` if not set.
    let starting_l2_block_number = match env::var("STARTING_L2_BLOCK_NUMBER") {
        Ok(n) => n.parse().unwrap(),
        Err(_) => {
            // Use finalized block minus the finality delay as a starting point
            let finalized_l2_header = data_fetcher.get_l2_header(BlockId::finalized()).await?;
            let finalized_l2_block = finalized_l2_header.number;

            let block_time = &data_fetcher
                .rollup_config
                .as_ref()
                .ok_or(anyhow::anyhow!("Rollup config not found"))?
                .block_time;

            let num_blocks_for_finality = dispute_game_finality_delay_seconds / block_time;
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

    let starting_block_number_hex = format!("0x{starting_l2_block_number:x}");
    let optimism_output_data: Value = data_fetcher
        .fetch_rpc_data_with_mode(
            RPCMode::L2Node,
            "optimism_outputAtBlock",
            vec![starting_block_number_hex.into()],
        )
        .await?;

    let starting_output_root = optimism_output_data["outputRoot"].as_str().unwrap().to_string();

    let fdg_config = FaultDisputeGameConfig {
        aggregation_vkey: shared_config.aggregation_vkey,
        challenger_addresses,
        challenger_bond_wei,
        dispute_game_finality_delay_seconds,
        fallback_timeout_fp_secs,
        game_type,
        initial_bond_wei,
        max_challenge_duration,
        max_prove_duration,
        optimism_portal2_address,
        permissionless_mode,
        proposer_addresses,
        range_vkey_commitment: shared_config.range_vkey_commitment,
        rollup_config_hash: shared_config.rollup_config_hash,
        starting_l2_block_number,
        starting_root: starting_output_root,
        use_sp1_mock_verifier: shared_config.use_sp1_mock_verifier,
        verifier_address: shared_config.verifier_address,
    };

    write_config_file(
        &fdg_config,
        &OP_SUCCINCT_FAULT_DISPUTE_GAME_CONFIG_PATH,
        "Fault Dispute Game",
    )?;

    Ok(())
}

use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Environment file to load
    #[arg(long, default_value = ".env")]
    env_file: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // This fetches the .env file from the project root. If the command is invoked in the contracts/
    // directory, the .env file in the root of the repo is used.
    if let Some(root) = find_project_root() {
        dotenv::from_path(root.join(args.env_file)).ok();
    } else {
        // Try to load the env file in case it's present
        if dotenv::from_path(args.env_file.clone()).is_err() {
            eprintln!("Warning: Could not find project root. {} file not loaded.", args.env_file);
        }
    }

    update_fdg_config().await?;

    Ok(())
}
