use std::env;
use std::str::FromStr;
use std::time::Duration;

use alloy::network::EthereumWallet;
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use alloy_primitives::{Address, B256};
use anyhow::Result;
use op_succinct_client_utils::types::u32_to_u8;
use op_succinct_host_utils::L2OutputOracle;
use reqwest::Url;
use sp1_sdk::{utils, HashableKey, ProverClient};

pub const AGG_ELF: &[u8] = include_bytes!("../../../elf/aggregation-elf");
pub const MULTI_BLOCK_ELF: &[u8] = include_bytes!("../../../elf/range-elf");

use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Contract address to check the vkey against.
    #[arg(short, long, required = false, value_delimiter = ',')]
    contract_addresses: Vec<String>,
}

// Get the verification keys for the ELFs and check them against the contract.
#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    utils::setup_logger();

    let args = Args::parse();

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
    let agg_vkey_b256 = B256::from_str(&agg_vk.bytes32()).unwrap();

    let private_key = env::var("PRIVATE_KEY").unwrap();
    let signer: PrivateKeySigner = private_key.parse().expect("Failed to parse private key");
    let wallet = EthereumWallet::from(signer);

    let l1_rpc = env::var("L1_RPC").unwrap();

    // Wait for 3 required confirmations with a timeout of 60 seconds.
    const NUM_CONFIRMATIONS: u64 = 3;
    const TIMEOUT_SECONDS: u64 = 60;

    for contract_address in args.contract_addresses {
        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(wallet.clone())
            .on_http(Url::parse(&l1_rpc).unwrap());

        let contract_address = Address::from_str(&contract_address).unwrap();
        let contract = L2OutputOracle::new(contract_address, provider);

        let receipt = contract
            .updateAggregationVKey(agg_vkey_b256)
            .send()
            .await?
            .with_required_confirmations(NUM_CONFIRMATIONS)
            .with_timeout(Some(Duration::from_secs(TIMEOUT_SECONDS)))
            .get_receipt()
            .await?;
        println!(
            "Updated Aggregation VKey on contract {}. Tx: {}",
            contract_address, receipt.transaction_hash
        );

        let receipt = contract
            .updateRangeVkeyCommitment(multi_block_vkey_b256)
            .send()
            .await?
            .with_required_confirmations(NUM_CONFIRMATIONS)
            .with_timeout(Some(Duration::from_secs(TIMEOUT_SECONDS)))
            .get_receipt()
            .await?;

        println!(
            "Updated Range VKey Commitment on contract {}. Tx: {}",
            contract_address, receipt.transaction_hash
        );
    }

    Ok(())
}
