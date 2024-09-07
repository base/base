use anyhow::Result;
use clap::Parser;
use kona_host::HostCli;
use kona_primitives::RollupConfig;
use log::info;
use op_succinct_host_utils::{
    fetcher::{CacheMode, ChainMode, OPSuccinctDataFetcher},
    get_proof_stdin,
    stats::{get_execution_stats, ExecutionStats},
    witnessgen::WitnessGenExecutor,
    ProgramType,
};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sp1_sdk::{utils, ProverClient};
use std::{
    cmp::{max, min},
    env, fs,
    future::Future,
    net::TcpListener,
    path::PathBuf,
    process::{Command, Stdio},
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

#[derive(Serialize)]
#[allow(non_snake_case)]
struct SpanBatchRequest {
    startBlock: u64,
    endBlock: u64,
    l2ChainId: u64,
    l2Node: String,
    l1Rpc: String,
    l1Beacon: String,
    batchSender: String,
}

#[derive(Deserialize, Debug, Clone)]
struct SpanBatchResponse {
    ranges: Option<Vec<SpanBatchRange>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpanBatchRange {
    start: u64,
    end: u64,
}

/// Get the span batches posted between the start and end blocks. Sends a request to a Go server
/// that runs a Span Batch Decoder.
async fn get_span_batch_ranges_from_server(
    data_fetcher: &OPSuccinctDataFetcher,
    start: u64,
    end: u64,
    l2_chain_id: u64,
    batch_sender: &str,
) -> Result<Vec<SpanBatchRange>> {
    let client = Client::new();
    let request = SpanBatchRequest {
        startBlock: start,
        endBlock: end,
        l2ChainId: l2_chain_id,
        l2Node: data_fetcher.l2_node_rpc.clone(),
        l1Rpc: data_fetcher.l1_rpc.clone(),
        l1Beacon: data_fetcher.l1_beacon_rpc.clone(),
        batchSender: batch_sender.to_string(),
    };

    // Get the span batch server URL from the environment.
    let span_batch_server_url =
        env::var("SPAN_BATCH_SERVER_URL").unwrap_or("http://localhost:8080".to_string());
    let query_url = format!("{}/span-batch-ranges", span_batch_server_url);

    let response: SpanBatchResponse =
        client.post(&query_url).json(&request).send().await?.json().await?;

    // If the response is empty, return one range with the start and end blocks.
    if response.ranges.is_none() {
        return Ok(vec![SpanBatchRange { start, end }]);
    }

    // Return the ranges.
    Ok(response.ranges.unwrap())
}

struct BatchHostCli {
    host_cli: HostCli,
    start: u64,
    end: u64,
}

fn get_max_span_batch_range_size(chain_id: u64) -> u64 {
    const DEFAULT_SIZE: u64 = 20;
    match chain_id {
        8453 => 5,      // Base
        11155111 => 20, // OP Sepolia
        10 => 10,       // OP Mainnet
        _ => DEFAULT_SIZE,
    }
}

/// Split ranges according to the max span batch range size per L2 chain.
fn split_ranges(span_batch_ranges: Vec<SpanBatchRange>, l2_chain_id: u64) -> Vec<SpanBatchRange> {
    let batch_size = get_max_span_batch_range_size(l2_chain_id);
    let mut split_ranges = Vec::new();

    for range in span_batch_ranges {
        if range.end - range.start > batch_size {
            let mut start = range.start;
            while start < range.end {
                let end = min(start + batch_size, range.end);
                split_ranges.push(SpanBatchRange { start, end });
                start = end;
            }
        } else {
            split_ranges.push(range);
        }
    }

    split_ranges
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
    let batch_host_clis = split_ranges.chunks(CONCURRENT_NATIVE_HOST_RUNNERS).map(|chunk| {
        let mut witnessgen_executor = WitnessGenExecutor::default();

        let mut batch_host_clis = Vec::new();
        for range in chunk.iter() {
            let host_cli = block_on(data_fetcher.get_host_cli_args(
                range.start,
                range.end,
                ProgramType::Multi,
                CacheMode::DeleteCache,
            ))
            .expect("Failed to get host CLI args.");

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
    host_clis: &[BatchHostCli],
    prover: &ProverClient,
    data_fetcher: &OPSuccinctDataFetcher,
) -> Vec<ExecutionStats> {
    host_clis
        .par_iter()
        .map(|r| {
            let sp1_stdin = get_proof_stdin(&r.host_cli).unwrap();

            let start_time = Instant::now();
            let (_, report) = prover.execute(MULTI_BLOCK_ELF, sp1_stdin).run().unwrap();
            let execution_duration = start_time.elapsed();
            block_on(get_execution_stats(data_fetcher, r.start, r.end, &report, execution_duration))
        })
        .collect()
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
        csv_writer.serialize(stats).expect("Failed to write execution stats to CSV.");
    }
    csv_writer.flush().expect("Failed to flush CSV writer.");

    Ok(())
}

/// Aggregate the execution statistics for an array of execution stats objects.
fn aggregate_execution_stats(execution_stats: &[ExecutionStats]) -> ExecutionStats {
    let mut aggregate_stats = ExecutionStats::default();
    let mut batch_start = u64::MAX;
    let mut batch_end = u64::MIN;
    for stats in execution_stats {
        batch_start = min(batch_start, stats.batch_start);
        batch_end = max(batch_end, stats.batch_end);

        // Accumulate most statistics across all blocks.
        aggregate_stats.execution_duration_sec += stats.execution_duration_sec;
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

    aggregate_stats
}

/// Build and manage the Docker container for the span batch server. Note: All logs are piped to
/// /dev/null, so the user doesn't see them.
fn manage_span_batch_server_container() -> Result<()> {
    // Check if port 8080 is already in use
    if TcpListener::bind("0.0.0.0:8080").is_err() {
        info!("Port 8080 is already in use. Assuming span_batch_server is running.");
        return Ok(());
    }

    // Build the Docker container if it doesn't exist.
    let build_status = Command::new("docker")
        .args([
            "build",
            "-t",
            "span_batch_server",
            "-f",
            "proposer/op/Dockerfile.span_batch_server",
            ".",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !build_status.success() {
        return Err(anyhow::anyhow!("Failed to build Docker container"));
    }

    // Start the Docker container.
    let run_status = Command::new("docker")
        .args(["run", "-p", "8080:8080", "-d", "span_batch_server"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !run_status.success() {
        return Err(anyhow::anyhow!("Failed to start Docker container"));
    }

    // Sleep for 5 seconds to allow the server to start.
    block_on(tokio::time::sleep(std::time::Duration::from_secs(5)));
    Ok(())
}

/// Shut down Docker container. Note: All logs are piped to /dev/null, so the user doesn't see them.
fn shutdown_span_batch_server_container() -> Result<()> {
    // Get the container ID associated with the span_batch_server image.
    let container_id = String::from_utf8(
        Command::new("docker")
            .args(["ps", "-q", "-f", "ancestor=span_batch_server"])
            .stdout(Stdio::piped())
            .output()?
            .stdout,
    )?
    .trim()
    .to_string();

    if container_id.is_empty() {
        return Ok(()); // Container not running, nothing to stop
    }

    // Stop the container.
    let stop_status = Command::new("docker")
        .args(["stop", &container_id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !stop_status.success() {
        return Err(anyhow::anyhow!("Failed to stop Docker container"));
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    utils::setup_logger();

    let args = HostArgs::parse();
    let data_fetcher = OPSuccinctDataFetcher::new();

    let l2_chain_id = data_fetcher.get_chain_id(ChainMode::L2).await?;
    let rollup_config = RollupConfig::from_l2_chain_id(l2_chain_id).unwrap();

    // Start the Docker container if it doesn't exist.
    manage_span_batch_server_container()?;

    let span_batch_ranges = get_span_batch_ranges_from_server(
        &data_fetcher,
        args.start,
        args.end,
        l2_chain_id,
        rollup_config.genesis.system_config.clone().unwrap().batcher_address.to_string().as_str(),
    )
    .await?;
    let split_ranges = split_ranges(span_batch_ranges, l2_chain_id);

    info!("The span batch ranges which will be executed: {:?}", split_ranges);

    let prover = ProverClient::new();
    let host_clis = run_native_data_generation(&data_fetcher, &split_ranges).await;

    let execution_stats = execute_blocks_parallel(&host_clis, &prover, &data_fetcher).await;

    // Sort the execution stats by batch start block.
    let mut sorted_execution_stats = execution_stats.clone();
    sorted_execution_stats.sort_by_key(|stats| stats.batch_start);
    write_execution_stats_to_csv(&sorted_execution_stats, l2_chain_id, &args)?;

    let aggregate_execution_stats = aggregate_execution_stats(&sorted_execution_stats);
    println!("Aggregate Execution Stats: \n {}", aggregate_execution_stats);

    // Shutdown the Docker container for fetching span batches.
    shutdown_span_batch_server_container()?;

    Ok(())
}
