use anyhow::Result;
use clap::Parser;
use op_succinct_host_utils::{
    block_range::get_validated_block_range,
    fetcher::{CacheMode, OPSuccinctDataFetcher, RunContext},
    get_proof_stdin, start_server_and_native_client,
    stats::ExecutionStats,
    ProgramType,
};
use op_succinct_prove::{execute_multi, DEFAULT_RANGE, RANGE_ELF};
use op_succinct_scripts::HostExecutorArgs;
use sp1_sdk::{utils, ProverClient};
use std::{fs, time::Instant};

/// Execute the OP Succinct program for multiple blocks.
#[tokio::main]
async fn main() -> Result<()> {
    let args = HostExecutorArgs::parse();

    dotenv::from_path(&args.env_file)?;
    utils::setup_logger();

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config(RunContext::Dev).await?;

    let cache_mode = if args.use_cache {
        CacheMode::KeepCache
    } else {
        CacheMode::DeleteCache
    };

    // If the end block is provided, check that it is less than the latest finalized block. If the end block is not provided, use the latest finalized block.
    let (l2_start_block, l2_end_block) =
        get_validated_block_range(&data_fetcher, args.start, args.end, DEFAULT_RANGE).await?;

    let host_args = data_fetcher
        .get_host_args(l2_start_block, l2_end_block, ProgramType::Multi, cache_mode)
        .await?;

    let start_time = Instant::now();
    let oracle = start_server_and_native_client(host_args.clone()).await?;
    let witness_generation_duration = start_time.elapsed();

    // Get the stdin for the block.
    let sp1_stdin = get_proof_stdin(oracle)?;

    let prover = ProverClient::from_env();

    if args.prove {
        // If the prove flag is set, generate a proof.
        let (pk, _) = prover.setup(RANGE_ELF);

        // Generate proofs in compressed mode for aggregation verification.
        let proof = prover.prove(&pk, &sp1_stdin).compressed().run().unwrap();

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
        let l2_chain_id = data_fetcher.get_l2_chain_id().await?;

        let (block_data, report, execution_duration) =
            execute_multi(&data_fetcher, sp1_stdin, l2_start_block, l2_end_block).await?;

        let l1_block_number = data_fetcher
            .get_l1_header(host_args.kona_args.l1_head.into())
            .await
            .unwrap()
            .number;
        let stats = ExecutionStats::new(
            l1_block_number,
            &block_data,
            &report,
            witness_generation_duration.as_secs(),
            execution_duration.as_secs(),
        );

        println!("Execution Stats: \n{:?}", stats);

        // Create the report directory if it doesn't exist.
        let report_dir = format!("execution-reports/multi/{}", l2_chain_id);
        if !std::path::Path::new(&report_dir).exists() {
            fs::create_dir_all(&report_dir)?;
        }

        let report_path = format!(
            "execution-reports/multi/{}/{}-{}.csv",
            l2_chain_id, l2_start_block, l2_end_block
        );

        // Write to CSV.
        let mut csv_writer = csv::Writer::from_path(report_path)?;
        csv_writer.serialize(&stats)?;
        csv_writer.flush()?;
    }

    Ok(())
}
