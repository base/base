use anyhow::Result;
use clap::Parser;
use kona_host::HostCli;
use log::info;
use op_succinct_host_utils::{
    fetcher::{CacheMode, OPSuccinctDataFetcher},
    get_proof_stdin,
    stats::ExecutionStats,
    witnessgen::WitnessGenExecutor,
    ProgramType,
};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use serde::{Deserialize, Serialize};
use sp1_sdk::{utils, ProverClient};
use std::{
    cmp::{max, min},
    collections::HashMap,
    fs::{self},
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::task::block_in_place;

pub const MULTI_BLOCK_ELF: &[u8] = include_bytes!("../../../elf/range-elf");

/// The arguments for the host executable.
#[derive(Debug, Clone, Parser)]
struct HostArgs {
    /// The start block of the range to execute.
    #[clap(long)]
    start: u64,
    /// The end block of the range to execute.
    #[clap(long)]
    end: u64,
    /// The number of blocks to execute in a single batch.
    #[clap(long, default_value = "5")]
    batch_size: usize,
    /// Whether to generate a proof or just execute the block.
    #[clap(long)]
    prove: bool,
    /// The path to the CSV file containing the execution data.
    #[clap(long, default_value = "report.csv")]
    report_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpanBatchRange {
    start: u64,
    end: u64,
}

struct BatchHostCli {
    host_cli: HostCli,
    start: u64,
    end: u64,
}

fn get_max_span_batch_range_size(l2_chain_id: u64) -> u64 {
    // TODO: The default size/batch size should be dynamic based on the L2 chain. Specifically, look at the gas used across the block range (should be fast to compute) and then set the batch size accordingly.
    const DEFAULT_SIZE: u64 = 1000;
    match l2_chain_id {
        8453 => 5,      // Base
        11155420 => 30, // OP Sepolia
        10 => 10,       // OP Mainnet
        _ => DEFAULT_SIZE,
    }
}

/// Split a range of blocks into a list of span batch ranges.
fn split_range(start: u64, end: u64, l2_chain_id: u64) -> Vec<SpanBatchRange> {
    let mut ranges = Vec::new();
    let mut current_start = start;
    let max_size = get_max_span_batch_range_size(l2_chain_id);

    while current_start < end {
        let current_end = min(current_start + max_size, end);
        ranges.push(SpanBatchRange {
            start: current_start,
            end: current_end,
        });
        current_start = current_end + 1;
    }

    ranges
}

/// Concurrently run the native data generation process for each split range.
async fn run_native_data_generation(
    data_fetcher: &OPSuccinctDataFetcher,
    split_ranges: &[SpanBatchRange],
) -> Vec<BatchHostCli> {
    const CONCURRENT_NATIVE_HOST_RUNNERS: usize = 5;

    // Split the entire range into chunks of size CONCURRENT_NATIVE_HOST_RUNNERS and process chunks
    // serially. Generate witnesses within each chunk in parallel. This prevents the RPC from
    // being overloaded with too many concurrent requests, while also improving witness generation
    // throughput.
    let batch_host_clis = split_ranges
        .chunks(CONCURRENT_NATIVE_HOST_RUNNERS)
        .map(|chunk| {
            let mut witnessgen_executor = WitnessGenExecutor::default();

            let mut batch_host_clis = Vec::new();
            for range in chunk.iter() {
                let host_cli = block_on(data_fetcher.get_host_cli_args(
                    range.start,
                    range.end,
                    ProgramType::Multi,
                    CacheMode::DeleteCache,
                ))
                .expect("Failed to get host CLI args");

                batch_host_clis.push(BatchHostCli {
                    host_cli: host_cli.clone(),
                    start: range.start,
                    end: range.end,
                });
                block_on(witnessgen_executor.spawn_witnessgen(&host_cli))
                    .expect("Failed to spawn witness generation process.");
            }

            let res = block_on(witnessgen_executor.flush());
            if res.is_err() {
                panic!("Failed to generate witnesses: {:?}", res.err().unwrap());
            }

            batch_host_clis
        });

    batch_host_clis.into_iter().flatten().collect()
}

/// Utility method for blocking on an async function.
///
/// If we're already in a tokio runtime, we'll block in place. Otherwise, we'll create a new
/// runtime.
pub fn block_on<T>(fut: impl Future<Output = T>) -> T {
    // Handle case if we're already in an tokio runtime.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        block_in_place(|| handle.block_on(fut))
    } else {
        // Otherwise create a new runtime.
        let rt = tokio::runtime::Runtime::new().expect("Failed to create a new runtime");
        rt.block_on(fut)
    }
}

/// Run the zkVM execution process for each split range in parallel.
async fn execute_blocks_parallel(
    host_clis: Vec<BatchHostCli>,
    prover: &ProverClient,
) -> Vec<ExecutionStats> {
    // Create a new execution stats map between the start and end block and the default ExecutionStats.
    let execution_stats_map = Arc::new(Mutex::new(HashMap::new()));

    // Fetch all of the execution stats block ranges in parallel.
    let mut handles = Vec::new();
    for (start, end) in host_clis.iter().map(|r| (r.start, r.end)) {
        let execution_stats_map = Arc::clone(&execution_stats_map);
        let handle = tokio::spawn(async move {
            // Create a new data fetcher. This avoids the runtime dropping the provider dispatch task.
            let data_fetcher = OPSuccinctDataFetcher::default();
            let mut exec_stats = ExecutionStats::default();
            exec_stats.add_block_data(&data_fetcher, start, end).await;
            let mut execution_stats_map = execution_stats_map.lock().unwrap();
            execution_stats_map.insert((start, end), exec_stats);
        });
        handles.push(handle);
    }
    futures::future::join_all(handles).await;

    // Run the zkVM execution process for each split range in parallel and fill in the execution stats.
    host_clis.par_iter().for_each(|r| {
        let sp1_stdin = get_proof_stdin(&r.host_cli).unwrap();

        // TODO: Implement retries with a smaller block range if this fails.
        let (_, report) = prover
            .execute(MULTI_BLOCK_ELF, sp1_stdin)
            .run()
            .unwrap_or_else(|e| {
                panic!(
                    "Failed to execute blocks {:?} - {:?}: {:?}",
                    r.start, r.end, e
                )
            });

        // Get the existing execution stats and modify it in place.
        let mut execution_stats_map = execution_stats_map.lock().unwrap();
        let exec_stats = execution_stats_map.get_mut(&(r.start, r.end)).unwrap();
        exec_stats.add_report_data(&report);
        exec_stats.add_aggregate_data();
    });

    info!("Execution is complete.");

    let execution_stats = execution_stats_map
        .lock()
        .unwrap()
        .clone()
        .into_values()
        .collect();
    drop(execution_stats_map);
    execution_stats
}

/// Write the execution stats to a CSV file.
fn write_execution_stats_to_csv(
    execution_stats: &[ExecutionStats],
    l2_chain_id: u64,
    args: &HostArgs,
) -> Result<()> {
    let report_path = PathBuf::from(format!(
        "execution-reports/{}/{}-{}-report.csv",
        l2_chain_id, args.start, args.end
    ));
    if let Some(parent) = report_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut csv_writer = csv::Writer::from_path(report_path)?;

    for stats in execution_stats {
        csv_writer
            .serialize(stats)
            .expect("Failed to write execution stats to CSV.");
    }
    csv_writer.flush().expect("Failed to flush CSV writer.");

    Ok(())
}

/// Aggregate the execution statistics for an array of execution stats objects.
fn aggregate_execution_stats(
    execution_stats: &[ExecutionStats],
    total_execution_time_sec: u64,
    witness_generation_time_sec: u64,
) -> ExecutionStats {
    let mut aggregate_stats = ExecutionStats::default();
    let mut batch_start = u64::MAX;
    let mut batch_end = u64::MIN;
    for stats in execution_stats {
        batch_start = min(batch_start, stats.batch_start);
        batch_end = max(batch_end, stats.batch_end);

        // Accumulate most statistics across all blocks.
        aggregate_stats.total_instruction_count += stats.total_instruction_count;
        aggregate_stats.oracle_verify_instruction_count += stats.oracle_verify_instruction_count;
        aggregate_stats.derivation_instruction_count += stats.derivation_instruction_count;
        aggregate_stats.block_execution_instruction_count +=
            stats.block_execution_instruction_count;
        aggregate_stats.blob_verification_instruction_count +=
            stats.blob_verification_instruction_count;
        aggregate_stats.total_sp1_gas += stats.total_sp1_gas;
        aggregate_stats.nb_blocks += stats.nb_blocks;
        aggregate_stats.nb_transactions += stats.nb_transactions;
        aggregate_stats.eth_gas_used += stats.eth_gas_used;
        aggregate_stats.bn_pair_cycles += stats.bn_pair_cycles;
        aggregate_stats.bn_add_cycles += stats.bn_add_cycles;
        aggregate_stats.bn_mul_cycles += stats.bn_mul_cycles;
        aggregate_stats.kzg_eval_cycles += stats.kzg_eval_cycles;
        aggregate_stats.ec_recover_cycles += stats.ec_recover_cycles;
    }

    // For statistics that are per-block or per-transaction, we take the average over the entire
    // range.
    aggregate_stats.cycles_per_block =
        aggregate_stats.total_instruction_count / aggregate_stats.nb_blocks;
    aggregate_stats.cycles_per_transaction =
        aggregate_stats.total_instruction_count / aggregate_stats.nb_transactions;
    aggregate_stats.transactions_per_block =
        aggregate_stats.nb_transactions / aggregate_stats.nb_blocks;
    aggregate_stats.gas_used_per_block = aggregate_stats.eth_gas_used / aggregate_stats.nb_blocks;
    aggregate_stats.gas_used_per_transaction =
        aggregate_stats.eth_gas_used / aggregate_stats.nb_transactions;

    // Use the earliest start and latest end across all blocks.
    aggregate_stats.batch_start = batch_start;
    aggregate_stats.batch_end = batch_end;

    // Set the total execution time to the total execution time of the entire range.
    aggregate_stats.total_execution_time_sec = total_execution_time_sec;
    aggregate_stats.witness_generation_time_sec = witness_generation_time_sec;

    aggregate_stats
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    utils::setup_logger();

    let args = HostArgs::parse();
    let data_fetcher = OPSuccinctDataFetcher::default();

    let l2_chain_id = data_fetcher.get_l2_chain_id().await?;

    let split_ranges = split_range(args.start, args.end, l2_chain_id);

    info!(
        "The span batch ranges which will be executed: {:?}",
        split_ranges
    );

    let prover = ProverClient::new();

    let start_time = Instant::now();
    let host_clis = run_native_data_generation(&data_fetcher, &split_ranges).await;
    let witness_generation_time_sec = start_time.elapsed().as_secs();

    let start_time = Instant::now();
    let execution_stats = execute_blocks_parallel(host_clis, &prover).await;
    let total_execution_time_sec = start_time.elapsed().as_secs();

    // Sort the execution stats by batch start block.
    let mut sorted_execution_stats = execution_stats.clone();
    sorted_execution_stats.sort_by_key(|stats| stats.batch_start);
    write_execution_stats_to_csv(&sorted_execution_stats, l2_chain_id, &args)?;

    let aggregate_execution_stats = aggregate_execution_stats(
        &sorted_execution_stats,
        total_execution_time_sec,
        witness_generation_time_sec,
    );
    println!(
        "Aggregate Execution Stats: \n {}",
        aggregate_execution_stats
    );

    Ok(())
}
