use alloy::sol;
use alloy_primitives::{hex, keccak256, B256};
use anyhow::Result;
use log::info;
use op_succinct_client_utils::types::u32_to_u8;
use sp1_sdk::{utils, HashableKey, ProverClient};

pub const AGG_ELF: &[u8] = include_bytes!("../../../elf/aggregation-elf");
pub const MULTI_BLOCK_ELF: &[u8] = include_bytes!("../../../elf/range-elf");

use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Contract address to check the vkey against.
    #[arg(short, long, required = false)]
    contract_address: Option<String>,

    /// RPC URL to use for the provider.
    #[arg(short, long, required = false)]
    rpc_url: Option<String>,
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    contract L2OutputOracle {
        bytes32 public vkey;
    }
}

// Get the verification keys for the ELFs and check them against the contract.
#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    utils::setup_logger();

    let prover = ProverClient::new();

    let (_, vkey) = prover.setup(MULTI_BLOCK_ELF);

    let program_hash = keccak256(MULTI_BLOCK_ELF);
    info!("Program Hash [view on Explorer]:");
    info!("0x{}", hex::encode(program_hash));

    println!("Range ELF Verification Key U32 Hash: {:?}", vkey.vk.hash_u32());

    // Get the 32 byte commitment to the vkey from vkey.vk.hash_u32()
    let multi_block_vkey_u8 = u32_to_u8(vkey.vk.hash_u32());
    let multi_block_vkey_b256 = B256::from(multi_block_vkey_u8);
    println!("Range ELF Verification Key B256: {}", multi_block_vkey_b256);

    let (_, agg_vk) = prover.setup(AGG_ELF);
    info!("Aggregation ELF Verification Key: {}", agg_vk.bytes32());
    println!("Aggregation ELF Verification Key: {}", agg_vk.bytes32());

    Ok(())
}
