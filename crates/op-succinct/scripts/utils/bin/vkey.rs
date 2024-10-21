use alloy_primitives::B256;
use anyhow::Result;
use op_succinct_client_utils::types::u32_to_u8;
use sp1_sdk::{utils, HashableKey, ProverClient};

pub const AGG_ELF: &[u8] = include_bytes!("../../../elf/aggregation-elf");
pub const MULTI_BLOCK_ELF: &[u8] = include_bytes!("../../../elf/range-elf");

// Get the verification keys for the ELFs and check them against the contract.
#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    utils::setup_logger();

    let prover = ProverClient::new();

    let (_, range_vk) = prover.setup(MULTI_BLOCK_ELF);

    // Get the 32 byte commitment to the vkey from vkey.vk.hash_u32()
    let multi_block_vkey_u8 = u32_to_u8(range_vk.vk.hash_u32());
    let multi_block_vkey_b256 = B256::from(multi_block_vkey_u8);
    println!(
        "Range ELF Verification Key Commitment: {}",
        multi_block_vkey_b256
    );

    let (_, agg_vk) = prover.setup(AGG_ELF);
    println!("Aggregation ELF Verification Key: {}", agg_vk.bytes32());

    Ok(())
}
