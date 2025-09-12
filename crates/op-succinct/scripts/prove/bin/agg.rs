use alloy_primitives::{Address, B256};
use anyhow::Result;
use cargo_metadata::MetadataCommand;
use clap::Parser;
use op_succinct_client_utils::{boot::BootInfoStruct, types::u32_to_u8};
use op_succinct_elfs::AGGREGATION_ELF;
use op_succinct_host_utils::{fetcher::OPSuccinctDataFetcher, get_agg_proof_stdin};
use op_succinct_proof_utils::get_range_elf_embedded;
use sp1_sdk::{
    utils, HashableKey, Prover, ProverClient, SP1Proof, SP1ProofWithPublicValues, SP1VerifyingKey,
};
use std::fs;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Start L2 block number.
    #[arg(short, long, num_args = 1.., value_delimiter = ',')]
    proofs: Vec<String>,

    /// Prove flag.
    #[arg(short, long)]
    prove: bool,

    /// Prover address.
    #[arg(short, long)]
    prover: Address,

    /// Env file path.
    #[arg(default_value = ".env", short, long)]
    env_file: String,
}

/// Load the aggregation proof data.
fn load_aggregation_proof_data(
    proof_names: Vec<String>,
    range_vkey: &SP1VerifyingKey,
) -> (Vec<SP1Proof>, Vec<BootInfoStruct>) {
    let metadata = MetadataCommand::new().exec().unwrap();
    let workspace_root = metadata.workspace_root;
    let proof_directory = format!("{workspace_root}/data/fetched_proofs");

    let mut proofs = Vec::with_capacity(proof_names.len());
    let mut boot_infos = Vec::with_capacity(proof_names.len());

    let prover = ProverClient::builder().cpu().build();

    for proof_name in proof_names.iter() {
        let proof_path = format!("{proof_directory}/{proof_name}.bin");
        if fs::metadata(&proof_path).is_err() {
            panic!("Proof file not found: {proof_path}");
        }
        let mut deserialized_proof =
            SP1ProofWithPublicValues::load(proof_path).expect("loading proof failed");
        prover.verify(&deserialized_proof, range_vkey).expect("proof verification failed");
        proofs.push(deserialized_proof.proof);

        // The public values are the BootInfoStruct.
        let boot_info = deserialized_proof.public_values.read();
        boot_infos.push(boot_info);
    }

    (proofs, boot_infos)
}

// Execute the OP Succinct program for a single block.
#[tokio::main]
async fn main() -> Result<()> {
    utils::setup_logger();

    let args = Args::parse();

    dotenv::from_filename(args.env_file).ok();

    let prover = ProverClient::from_env();
    let fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

    let (_, vkey) = prover.setup(get_range_elf_embedded());

    let (proofs, boot_infos) = load_aggregation_proof_data(args.proofs, &vkey);

    let header = fetcher.get_latest_l1_head_in_batch(&boot_infos).await?;
    let headers = fetcher.get_header_preimages(&boot_infos, header.hash_slow()).await?;
    let multi_block_vkey_u8 = u32_to_u8(vkey.vk.hash_u32());
    let multi_block_vkey_b256 = B256::from(multi_block_vkey_u8);
    println!("Range ELF Verification Key Commitment: {multi_block_vkey_b256}");
    let stdin =
        get_agg_proof_stdin(proofs, boot_infos, headers, &vkey, header.hash_slow(), args.prover)
            .expect("Failed to get agg proof stdin");

    let (agg_pk, agg_vk) = prover.setup(AGGREGATION_ELF);
    println!("Aggregate ELF Verification Key: {:?}", agg_vk.vk.bytes32());

    if args.prove {
        prover.prove(&agg_pk, &stdin).groth16().run().expect("proving failed");
    } else {
        let (_, report) = prover
            .execute(AGGREGATION_ELF, &stdin)
            .calculate_gas(true)
            .deferred_proof_verification(false)
            .run()
            .unwrap();
        println!("report: {report:?}");
    }

    Ok(())
}
