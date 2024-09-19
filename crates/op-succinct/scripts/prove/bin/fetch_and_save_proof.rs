use alloy::{hex, sol_types::SolValue};
use anyhow::Result;
use clap::Parser;
use dotenv::dotenv;
use op_succinct_client_utils::{boot::BootInfoStruct, BOOT_INFO_SIZE};
use sp1_sdk::{NetworkProver, SP1ProofWithPublicValues};
use std::{fs, path::Path};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Request ID string
    #[arg(short, long)]
    request_id: String,

    /// Chain ID
    ///
    /// 10 for OP
    /// 11155420 for OP Sepolia
    #[arg(short, long)]
    chain_id: u64,

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
    dotenv().ok();
    let args = Args::parse();

    let prover = NetworkProver::new();

    // Fetch the proof
    let mut proof: SP1ProofWithPublicValues = prover.wait_proof(&args.request_id, None).await?;

    if args.agg_proof {
        let mut raw_boot_info = [0u8; BOOT_INFO_SIZE];
        proof.public_values.read_slice(&mut raw_boot_info);
        let boot_info = BootInfoStruct::abi_decode(&raw_boot_info, false).unwrap();

        let proof_bytes = proof.bytes();
        println!("Proof bytes: {:?}", hex::encode(proof_bytes));
        println!("Boot info: {:?}", boot_info);
    } else {
        // Create the proofs directory if it doesn't exist
        let proof_path = format!("data/{}/proofs", args.chain_id);
        let proof_dir = Path::new(&proof_path);
        fs::create_dir_all(proof_dir)?;

        // Generate the filename
        let filename = format!("{}-{}.bin", args.start.unwrap(), args.end.unwrap());
        let file_path = proof_dir.join(filename);

        // Save the proof
        proof.save(file_path).expect("Failed to save proof");

        println!(
            "Proof saved successfully for blocks {} to {}",
            args.start.unwrap(),
            args.end.unwrap()
        );
    }

    Ok(())
}
