use alloy_primitives::B256;
use anyhow::Result;
use clap::Parser;
use op_succinct_client_utils::{boot::hash_rollup_config, types::u32_to_u8};
use op_succinct_host_utils::{fetcher::OPSuccinctDataFetcher, AGGREGATION_ELF, RANGE_ELF_EMBEDDED};
use op_succinct_scripts::ConfigArgs;
use sp1_sdk::{utils, HashableKey, Prover, ProverClient};
// Get the verification keys for the ELFs and check them against the contract.
#[tokio::main]
async fn main() -> Result<()> {
    let args = ConfigArgs::parse();

    let prover = ProverClient::builder().cpu().build();

    let (_, range_vk) = prover.setup(RANGE_ELF_EMBEDDED);

    // Get the 32 byte commitment to the vkey from vkey.vk.hash_u32()
    let multi_block_vkey_u8 = u32_to_u8(range_vk.vk.hash_u32());
    let multi_block_vkey_b256 = B256::from(multi_block_vkey_u8);
    println!(
        "Range ELF Verification Key Commitment: {}",
        multi_block_vkey_b256
    );

    let (_, agg_vk) = prover.setup(AGGREGATION_ELF);
    println!("Aggregation ELF Verification Key: {}", agg_vk.bytes32());

    if let Some(env_file) = args.env_file {
        dotenv::from_path(env_file).ok();

        utils::setup_logger();

        let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

        let rollup_config = data_fetcher.rollup_config.as_ref().unwrap();
        println!(
            "Rollup Config Hash: 0x{:x}",
            hash_rollup_config(rollup_config)
        );
    }

    Ok(())
}
