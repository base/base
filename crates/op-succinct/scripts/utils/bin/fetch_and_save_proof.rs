use alloy::{hex, sol_types::SolValue};
use anyhow::Result;
use clap::Parser;
use op_succinct_client_utils::{boot::BootInfoStruct, AGGREGATION_OUTPUTS_SIZE};
use sp1_sdk::{NetworkProverV1, SP1ProofWithPublicValues};
use std::{fs, path::Path};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Request ID string
    #[arg(short, long)]
    request_id: String,

    /// Aggregate proof.
    #[arg(short, long)]
    agg_proof: bool,

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

    let prover = NetworkProverV1::new();

    // Fetch the proof
    let mut proof: SP1ProofWithPublicValues = prover.wait_proof(&args.request_id, None).await?;

    if args.agg_proof {
        let mut raw_boot_info = [0u8; AGGREGATION_OUTPUTS_SIZE];
        proof.public_values.read_slice(&mut raw_boot_info);
        let boot_info = BootInfoStruct::abi_decode(&raw_boot_info, false).unwrap();

        let proof_bytes = proof.bytes();
        println!("Proof bytes: {:?}", hex::encode(proof_bytes));
        println!("Boot info: {:?}", boot_info);
    } else {
        // Read the BootInfoStruct from the proof
        let _boot_info: BootInfoStruct = proof.public_values.read();

        // Create the proofs directory if it doesn't exist
        let proof_path = "data/fetched_proofs".to_string();
        let proof_dir = Path::new(&proof_path);
        fs::create_dir_all(proof_dir)?;

        let filename: String = if args.start.is_some() && args.end.is_some() {
            let start = args.start.unwrap();
            let end = args.end.unwrap();
            format!("{}_{}.bin", start, end)
        } else {
            // Generate the filename
            format!("{}.bin", args.request_id)
        };
        let file_path = proof_dir.join(filename);

        // Save the proof
        proof.save(&file_path).expect("Failed to save proof");

        println!(
            "Proof saved successfully to path: {}",
            file_path.to_str().unwrap()
        );
    }

    Ok(())
}
