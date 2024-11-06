use anyhow::Result;
use clap::Parser;
use op_succinct_host_utils::{
    fetcher::{CacheMode, OPSuccinctDataFetcher},
    get_proof_stdin,
    stats::ExecutionStats,
    witnessgen::WitnessGenExecutor,
    ProgramType,
};
use sp1_sdk::{utils, ProverClient};
use std::time::Instant;

pub const SINGLE_BLOCK_ELF: &[u8] = include_bytes!("../../../elf/fault-proof-elf");

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Start block number.
    #[arg(short, long)]
    l2_block: u64,

    /// Skip running native execution.
    #[arg(short, long)]
    use_cache: bool,

    /// Generate proof.
    #[arg(short, long)]
    prove: bool,
}

/// Execute the OP Succinct program for a single block.
#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    let args = Args::parse();
    utils::setup_logger();

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config()
        .await
        .unwrap();

    let l2_chain_id = data_fetcher.get_l2_chain_id().await?;

    let l2_safe_head = args.l2_block - 1;

    let cache_mode = if args.use_cache {
        CacheMode::KeepCache
    } else {
        CacheMode::DeleteCache
    };

    let host_cli = data_fetcher
        .get_host_cli_args(l2_safe_head, args.l2_block, ProgramType::Single, cache_mode)
        .await?;

    // By default, re-run the native execution unless the user passes `--use-cache`.
    let start_time = Instant::now();
    if !args.use_cache {
        // Start the server and native client.
        let mut witnessgen_executor = WitnessGenExecutor::default();
        witnessgen_executor.spawn_witnessgen(&host_cli).await?;
        witnessgen_executor.flush().await?;
    }
    let witness_generation_time_sec = start_time.elapsed();
    // Get the stdin for the block.
    let sp1_stdin = get_proof_stdin(&host_cli)?;

    let prover = ProverClient::new();

    if args.prove {
        // If the prove flag is set, generate a proof.
        let (pk, _) = prover.setup(SINGLE_BLOCK_ELF);

        // Generate proofs in PLONK mode for on-chain verification.
        let proof = prover.prove(&pk, sp1_stdin).plonk().run().unwrap();

        // Create a proof directory for the chain ID if it doesn't exist.
        let proof_dir = format!("data/{}/proofs", l2_chain_id);
        if !std::path::Path::new(&proof_dir).exists() {
            std::fs::create_dir_all(&proof_dir)?;
        }
        proof
            .save(format!("{}/{}.bin", proof_dir, args.l2_block))
            .expect("Failed to save proof");
    } else {
        let start_time = Instant::now();
        let (_, report) = prover.execute(SINGLE_BLOCK_ELF, sp1_stdin).run().unwrap();
        let execution_duration = start_time.elapsed();

        let report_path = format!(
            "execution-reports/single/{}/{}.csv",
            l2_chain_id, args.l2_block
        );

        // Create the report directory if it doesn't exist.
        let report_dir = format!("execution-reports/single/{}", l2_chain_id);
        if !std::path::Path::new(&report_dir).exists() {
            std::fs::create_dir_all(&report_dir)?;
        }

        let block_data = data_fetcher
            .get_l2_block_data_range(args.l2_block, args.l2_block)
            .await?;

        let stats = ExecutionStats::new(
            &block_data,
            &report,
            witness_generation_time_sec.as_secs(),
            execution_duration.as_secs(),
        );
        println!("Execution Stats: \n{:?}", stats);

        // Write to CSV.
        let mut csv_writer = csv::Writer::from_path(report_path)?;
        csv_writer.serialize(&stats)?;
        csv_writer.flush()?;
    }

    Ok(())
}
