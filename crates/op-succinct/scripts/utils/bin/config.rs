use alloy_primitives::B256;
use anyhow::Result;
use clap::Parser;
use op_succinct_client_utils::{boot::hash_rollup_config, types::u32_to_u8};
use op_succinct_host_utils::fetcher::OPSuccinctDataFetcher;
use op_succinct_proof_utils::{get_range_elf_embedded, AGGREGATION_ELF};
use op_succinct_scripts::ConfigArgs;
use sp1_sdk::{utils, HashableKey, Prover, ProverClient};

// Get the verification keys for the ELFs and check them against the contract.
#[tokio::main]
async fn main() -> Result<()> {
    let args = ConfigArgs::parse();

    let prover = ProverClient::builder().cpu().build();

    let (_, range_vk) = prover.setup(get_range_elf_embedded());

    // Get the 32 byte commitment to the vkey from vkey.vk.hash_u32()
    let range_vk_hash = B256::from(u32_to_u8(range_vk.vk.hash_u32()));
    println!("Range Verification Key Hash: {range_vk_hash}");

    let (_, agg_vk) = prover.setup(AGGREGATION_ELF);
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
