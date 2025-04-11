use anyhow::{Context, Result};
use clap::Parser;
use op_succinct_host_utils::{
    block_range::get_validated_block_range,
    fetcher::OPSuccinctDataFetcher,
    get_proof_stdin,
    hosts::{default::SingleChainOPSuccinctHost, OPSuccinctHost},
    stats::ExecutionStats,
    RANGE_ELF_EMBEDDED,
};
use op_succinct_prove::{execute_multi, DEFAULT_RANGE};
use op_succinct_scripts::HostExecutorArgs;
use sp1_sdk::{utils, ProverClient};
use std::{fs, sync::Arc, time::Instant};
use tracing::debug;

/// Execute the OP Succinct program for multiple blocks.
#[tokio::main]
async fn main() -> Result<()> {
    let args = HostExecutorArgs::parse();

    dotenv::from_path(&args.env_file)
        .context(format!("Environment file not found: {}", args.env_file.display()))?;
    utils::setup_logger();

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

    // If the end block is provided, check that it is less than the latest finalized block. If the
    // end block is not provided, use the latest finalized block.
    let (l2_start_block, l2_end_block) =
        get_validated_block_range(&data_fetcher, args.start, args.end, DEFAULT_RANGE).await?;

    let host = SingleChainOPSuccinctHost { fetcher: Arc::new(data_fetcher.clone()) };
    let host_args =
        host.fetch(l2_start_block, l2_end_block, None, Some(args.safe_db_fallback)).await?;

    debug!("Host args: {:?}", host_args);

    let start_time = Instant::now();
    let oracle = host.run(&host_args).await?;
    let witness_generation_duration = start_time.elapsed();

    // Get the stdin for the block.
    let sp1_stdin = get_proof_stdin(oracle)?;

    let prover = ProverClient::from_env();

    if args.prove {
        // If the prove flag is set, generate a proof.
        let (pk, _) = prover.setup(RANGE_ELF_EMBEDDED);

        // Generate proofs in compressed mode for aggregation verification.
        let proof = prover.prove(&pk, &sp1_stdin).compressed().run().unwrap();

        // Create a proof directory for the chain ID if it doesn't exist.
        let proof_dir = format!("data/{}/proofs", data_fetcher.get_l2_chain_id().await.unwrap());
        if !std::path::Path::new(&proof_dir).exists() {
            fs::create_dir_all(&proof_dir).unwrap();
        }
        // Save the proof to the proof directory corresponding to the chain ID.
        proof
            .save(format!("{}/{}-{}.bin", proof_dir, l2_start_block, l2_end_block))
            .expect("saving proof failed");
    } else {
        let l2_chain_id = data_fetcher.get_l2_chain_id().await?;

        let (block_data, report, execution_duration) =
            execute_multi(&data_fetcher, sp1_stdin, l2_start_block, l2_end_block).await?;

        let l1_block_number =
            data_fetcher.get_l1_header(host_args.l1_head.into()).await.unwrap().number;
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
