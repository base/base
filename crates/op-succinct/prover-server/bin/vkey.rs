use std::str::FromStr;

use alloy::{providers::ProviderBuilder, sol, transports::http::reqwest::Url};
use alloy_primitives::{hex, keccak256, Address, B256};
use anyhow::Result;
use log::info;
use sp1_sdk::{utils, HashableKey, ProverClient};

pub const AGG_ELF: &[u8] = include_bytes!("../../elf/aggregation-elf");
pub const MULTI_BLOCK_ELF: &[u8] = include_bytes!("../../elf/range-elf");

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

    let args = Args::parse();

    let prover = ProverClient::new();

    let (_, vkey) = prover.setup(MULTI_BLOCK_ELF);

    let program_hash = keccak256(MULTI_BLOCK_ELF);
    info!("Program Hash [view on Explorer]:");
    info!("0x{}", hex::encode(program_hash));

    info!(
        "Multi-block ELF Verification Key U32 Hash: {:?}",
        vkey.vk.hash_u32()
    );

    let (_, agg_vk) = prover.setup(AGG_ELF);
    info!("Aggregation ELF Verification Key: {}", agg_vk.bytes32());
    let agg_vk_bytes: [u8; 32] = hex::decode(agg_vk.bytes32().replace("0x", ""))
        .unwrap()
        .try_into()
        .unwrap();

    // Check the aggregate vkey against the contract.
    let provider = ProviderBuilder::new().on_http(Url::from_str(&args.rpc_url.unwrap()).unwrap());

    let contract = L2OutputOracle::new(
        Address::from_str(&args.contract_address.unwrap()).unwrap(),
        provider,
    );
    let vkey = contract.vkey().call().await?;

    assert_eq!(vkey.vkey, B256::from(agg_vk_bytes));
    info!("The verification key matches the contract.");

    Ok(())
}
