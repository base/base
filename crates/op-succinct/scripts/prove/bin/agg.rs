use alloy_primitives::{Address, B256};
use anyhow::{Context, Result};
use cargo_metadata::MetadataCommand;
use clap::Parser;
use op_succinct_client_utils::{boot::BootInfoStruct, types::u32_to_u8};
use op_succinct_elfs::AGGREGATION_ELF;
use op_succinct_host_utils::{fetcher::OPSuccinctDataFetcher, get_agg_proof_stdin};
use op_succinct_proof_utils::{
    cluster_agg_proof, cluster_setup_keys, get_range_elf_embedded, is_cluster_mode,
};
use sp1_sdk::{
    blocking::{self, Prover as BlockingProver},
    utils, Elf, HashableKey, ProveRequest, Prover, ProverClient, ProvingKey, SP1Proof,
    SP1ProofMode, SP1ProofWithPublicValues, SP1Stdin, SP1VerifyingKey,
};
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

    /// Cluster proving timeout in seconds (only used when SP1_PROVER=cluster).
    #[arg(long, default_value = "21600")]
    cluster_timeout: u64,
}

/// Load the aggregation proof data.
///
/// Uses `spawn_blocking` because `blocking::CpuProver` creates its own tokio runtime
/// internally, which would panic if called directly from an async context.
async fn load_aggregation_proof_data(
    proof_names: Vec<String>,
    range_vkey: SP1VerifyingKey,
) -> (Vec<SP1Proof>, Vec<BootInfoStruct>) {
    tokio::task::spawn_blocking(move || {
        let metadata = MetadataCommand::new().exec().unwrap();
        let workspace_root = metadata.workspace_root;
        let proof_directory = format!("{workspace_root}/data/fetched_proofs");

        let mut proofs = Vec::with_capacity(proof_names.len());
        let mut boot_infos = Vec::with_capacity(proof_names.len());

        let prover = blocking::CpuProver::new();

        for proof_name in proof_names.iter() {
            let proof_path = format!("{proof_directory}/{proof_name}.bin");
            let mut deserialized_proof =
                SP1ProofWithPublicValues::load(proof_path).expect("loading proof failed");
            prover
                .verify(&deserialized_proof, &range_vkey, None)
                .expect("proof verification failed");
            proofs.push(deserialized_proof.proof);

            let boot_info = deserialized_proof.public_values.read();
            boot_infos.push(boot_info);
        }

        (proofs, boot_infos)
    })
    .await
    .expect("load_aggregation_proof_data task panicked")
}

async fn build_agg_stdin(
    fetcher: &OPSuccinctDataFetcher,
    proof_names: Vec<String>,
    range_vkey: &SP1VerifyingKey,
    prover_address: Address,
) -> Result<SP1Stdin> {
    let (proofs, boot_infos) = load_aggregation_proof_data(proof_names, range_vkey.clone()).await;

    let header = fetcher.get_latest_l1_head_in_batch(&boot_infos).await?;
    let l1_head_hash = header.hash_slow();
    let headers = fetcher.get_header_preimages(&boot_infos, l1_head_hash).await?;
    let multi_block_vkey_u8 = u32_to_u8(range_vkey.vk.hash_u32());
    let multi_block_vkey_b256 = B256::from(multi_block_vkey_u8);
    println!("Range ELF Verification Key Commitment: {multi_block_vkey_b256}");

    let stdin =
        get_agg_proof_stdin(proofs, boot_infos, headers, range_vkey, l1_head_hash, prover_address)?;

    Ok(stdin)
}

// Execute the OP Succinct program for a single block.
#[tokio::main]
async fn main() -> Result<()> {
    utils::setup_logger();

    let args = Args::parse();

    dotenv::from_filename(args.env_file).ok();

    let fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

    if is_cluster_mode() {
        let (_range_pk, vkey, _agg_pk, agg_vk) = cluster_setup_keys().await?;

        println!("Aggregate ELF Verification Key: {:?}", agg_vk.bytes32());

        let stdin = build_agg_stdin(&fetcher, args.proofs, &vkey, args.prover).await?;

        if args.prove {
            let proof =
                cluster_agg_proof(args.cluster_timeout, SP1ProofMode::Groth16, stdin).await?;
            proof.save("output.bin").context("saving proof failed")?;
        } else {
            // CpuProver creates its own tokio runtime, so run execution outside the async context.
            let (_, report) = tokio::task::spawn_blocking(move || {
                let cpu_prover = blocking::CpuProver::new();
                cpu_prover
                    .execute(Elf::Static(AGGREGATION_ELF), stdin)
                    .calculate_gas(true)
                    .deferred_proof_verification(false)
                    .run()
            })
            .await??;
            println!("report: {report:?}");
        }
    } else {
        let prover = ProverClient::from_env().await;

        let range_pk = prover.setup(Elf::Static(get_range_elf_embedded())).await?;
        let vkey = range_pk.verifying_key().clone();

        let stdin = build_agg_stdin(&fetcher, args.proofs, &vkey, args.prover).await?;

        let agg_pk = prover.setup(Elf::Static(AGGREGATION_ELF)).await?;
        let agg_vk = agg_pk.verifying_key();
        println!("Aggregate ELF Verification Key: {:?}", agg_vk.bytes32());

        if args.prove {
            prover.prove(&agg_pk, stdin).groth16().await.expect("proving failed");
        } else {
            let (_, report) = prover
                .execute(Elf::Static(AGGREGATION_ELF), stdin)
                .calculate_gas(true)
                .deferred_proof_verification(false)
                .await
                .unwrap();
            println!("report: {report:?}");
        }
    }

    Ok(())
}
