//! Binary for generating and displaying succinct configuration values.

use alloy_primitives::B256;
use anyhow::Result;
use base_proof_succinct_client_utils::{boot::hash_rollup_config, types::u32_to_u8};
use base_proof_succinct_host_utils::fetcher::OPSuccinctDataFetcher;
use base_proof_succinct_proof_utils::cluster_setup_vkeys;
use base_proof_succinct_scripts::ConfigArgs;
use clap::Parser;
use sp1_sdk::{HashableKey, utils};

// Get the verification keys for the ELFs and check them against the contract.
#[tokio::main]
async fn main() -> Result<()> {
    let args = ConfigArgs::parse();

    let (range_vk, agg_vk) = cluster_setup_vkeys().await?;

    // Get the 32 byte commitment to the vkey from hash_u32()
    let range_vk_hash = B256::from(u32_to_u8(range_vk.hash_u32()));
    println!("Range Verification Key Hash: {range_vk_hash}");

    println!("Aggregation Verification Key Hash: {}", agg_vk.bytes32());

    if let Some(env_file) = args.env_file {
        dotenv::from_path(env_file).ok();

        utils::setup_logger();

        let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

        let rollup_config = data_fetcher.rollup_config.as_ref().unwrap();
        println!("Rollup Config Hash: 0x{:x}", hash_rollup_config(rollup_config));
    }

    Ok(())
}
