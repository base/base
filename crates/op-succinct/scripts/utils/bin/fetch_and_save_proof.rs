use alloy_primitives::{hex, B256};
use alloy_sol_types::SolValue;
use anyhow::Result;
use clap::Parser;
use op_succinct_client_utils::{boot::BootInfoStruct, AGGREGATION_OUTPUTS_SIZE};
use op_succinct_host_utils::proof_cache::{get_range_proof_dir, save_range_proof};
use sp1_sdk::{
    network::proto::{
        types::{ExecutionStatus, FulfillmentStatus},
        GetProofRequestStatusResponse,
    },
    ProverClient, SP1ProofWithPublicValues,
};
use std::fs;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Request ID string
    #[arg(short, long)]
    request_id: String,

    /// Aggregate proof.
    #[arg(short, long)]
    agg_proof: bool,

    /// L2 chain ID.
    #[arg(short, long)]
    chain_id: u64,

    /// Start L2 block number.
    #[arg(short, long, required = false)]
    start: Option<u64>,

    /// End L2 block number.
    #[arg(short, long, required = false)]
    end: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    let args = Args::parse();

    let prover = ProverClient::builder().network().build().await;

    let request_id = hex::decode(&args.request_id)?;
    // Fetch the proof
    let (status, proof): (GetProofRequestStatusResponse, Option<SP1ProofWithPublicValues>) =
        prover.get_proof_status(B256::from_slice(&request_id)).await?;
    let fulfillment_status = FulfillmentStatus::try_from(status.fulfillment_status()).unwrap();
    let _ = ExecutionStatus::try_from(status.execution_status()).unwrap();

    let mut proof = match fulfillment_status {
        FulfillmentStatus::Fulfilled => proof.unwrap(),
        _ => {
            println!("Proof is still pending");
            return Ok(());
        }
    };

    if args.agg_proof {
        let mut raw_boot_info = [0u8; AGGREGATION_OUTPUTS_SIZE];
        proof.public_values.read_slice(&mut raw_boot_info);
        let boot_info = BootInfoStruct::abi_decode(&raw_boot_info).unwrap();

        let proof_bytes = proof.bytes();
        println!("Proof bytes: {:?}", hex::encode(proof_bytes));
        println!("Boot info: {boot_info:?}");
    } else {
        // Read the BootInfoStruct from the proof
        let _boot_info: BootInfoStruct = proof.public_values.read();

        let file_path = if let (Some(start), Some(end)) = (args.start, args.end) {
            save_range_proof(args.chain_id, start, end, &proof)?
        } else {
            let dir = get_range_proof_dir(args.chain_id);
            fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{}.bin", args.request_id));
            proof.save(&path).expect("Failed to save proof");
            path
        };

        println!("Proof saved successfully to path: {}", file_path.display());
    }

    Ok(())
}
