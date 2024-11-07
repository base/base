use anyhow::Result;
use clap::Parser;
use op_succinct_host_utils::{
    block_range::get_validated_block_range,
    fetcher::{CacheMode, OPSuccinctDataFetcher},
    get_proof_stdin,
    stats::ExecutionStats,
    witnessgen::WitnessGenExecutor,
    ProgramType,
};
use sp1_sdk::{utils, ProverClient};
use std::{fs, time::Instant};

pub const MULTI_BLOCK_ELF: &[u8] = include_bytes!("../../../elf/range-elf");

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Start L2 block number.
    #[arg(short, long)]
    start: Option<u64>,

    /// End L2 block number.
    #[arg(short, long)]
    end: Option<u64>,

    /// Verbosity level.
    #[arg(short, long, default_value = "0")]
    verbosity: u8,

    /// Skip running native execution.
    #[arg(short, long)]
    use_cache: bool,

    /// Generate proof.
    #[arg(short, long)]
    prove: bool,

    /// Env file.
    #[arg(short, long, default_value = ".env")]
    env_file: Option<String>,
}

/// Execute the OP Succinct program for multiple blocks.
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if let Some(env_file) = args.env_file {
        dotenv::from_filename(env_file).ok();
    }
    utils::setup_logger();

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config()
        .await
        .unwrap();

    let cache_mode = if args.use_cache {
        CacheMode::KeepCache
    } else {
        CacheMode::DeleteCache
    };

    const DEFAULT_RANGE: u64 = 5;

    // If the end block is provided, check that it is less than the latest finalized block. If the end block is not provided, use the latest finalized block.
    let (l2_start_block, l2_end_block) =
        get_validated_block_range(&data_fetcher, args.start, args.end, DEFAULT_RANGE).await?;

    let host_cli = data_fetcher
        .get_host_cli_args(l2_start_block, l2_end_block, ProgramType::Multi, cache_mode)
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
    println!(
        "Witness Generation Duration: {:?}",
        witness_generation_time_sec.as_secs()
    );

    // Get the stdin for the block.
    let sp1_stdin = get_proof_stdin(&host_cli)?;

    let prover = ProverClient::new();

    if args.prove {
        // If the prove flag is set, generate a proof.
        let (pk, _) = prover.setup(MULTI_BLOCK_ELF);

        // Generate proofs in compressed mode for aggregation verification.
        let proof = prover.prove(&pk, sp1_stdin).compressed().run().unwrap();

        // Create a proof directory for the chain ID if it doesn't exist.
        let proof_dir = format!(
            "data/{}/proofs",
            data_fetcher.get_l2_chain_id().await.unwrap()
        );
        if !std::path::Path::new(&proof_dir).exists() {
            fs::create_dir_all(&proof_dir).unwrap();
        }
        // Save the proof to the proof directory corresponding to the chain ID.
        proof
            .save(format!(
                "{}/{}-{}.bin",
                proof_dir, l2_start_block, l2_end_block
            ))
            .expect("saving proof failed");
    } else {
        let start_time = Instant::now();
        let (_, report) = prover
            .execute(MULTI_BLOCK_ELF, sp1_stdin.clone())
            .run()
            .unwrap();
        let execution_duration = start_time.elapsed();

        let l2_chain_id = data_fetcher.get_l2_chain_id().await.unwrap();
        let report_path = format!(
            "execution-reports/multi/{}/{}-{}.csv",
            l2_chain_id, l2_start_block, l2_end_block
        );

        // Create the report directory if it doesn't exist.
        let report_dir = format!("execution-reports/multi/{}", l2_chain_id);
        if !std::path::Path::new(&report_dir).exists() {
            fs::create_dir_all(&report_dir).unwrap();
        }

        let block_data = data_fetcher
            .get_l2_block_data_range(l2_start_block, l2_end_block)
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
