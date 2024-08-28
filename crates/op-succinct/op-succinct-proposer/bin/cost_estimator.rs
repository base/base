use anyhow::Result;
use clap::Parser;
use host_utils::{
    fetcher::{ChainMode, SP1KonaDataFetcher},
    get_proof_stdin,
    stats::{get_execution_stats, ExecutionStats},
    ProgramType,
};
use kona_host::HostCli;
use kona_primitives::RollupConfig;
use log::{error, info};
use op_succinct_proposer::run_native_host;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sp1_sdk::{utils, ProverClient};
use std::{
    cmp::min,
    env, fs,
    future::Future,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::task::block_in_place;

pub const MULTI_BLOCK_ELF: &[u8] = include_bytes!("../../elf/range-elf");

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
    ranges: Vec<SpanBatchRange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpanBatchRange {
    start: u64,
    end: u64,
}

/// Get the span batches posted between the start and end blocks. Sends a request to a Go server
/// that runs a Span Batch Decoder.
async fn get_span_batch_ranges_from_server(
    data_fetcher: &SP1KonaDataFetcher,
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

    let response: SpanBatchResponse = client
        .post(&query_url)
        .json(&request)
        .send()
        .await?
        .json()
        .await?;

    // Return the ranges.
    Ok(response.ranges)
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

async fn fetch_span_batch_ranges(
    data_fetcher: &SP1KonaDataFetcher,
    args: &HostArgs,
    l2_chain_id: u64,
    rollup_config: &RollupConfig,
) -> Result<Vec<SpanBatchRange>> {
    get_span_batch_ranges_from_server(
        data_fetcher,
        args.start,
        args.end,
        l2_chain_id,
        rollup_config
            .genesis
            .system_config
            .clone()
            .unwrap()
            .batcher_address
            .to_string()
            .as_str(),
    )
    .await
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
    data_fetcher: &SP1KonaDataFetcher,
    split_ranges: &[SpanBatchRange],
) -> Vec<BatchHostCli> {
    const CONCURRENT_NATIVE_HOST_RUNNERS: usize = 5;
    const NATIVE_HOST_TIMEOUT: Duration = Duration::from_secs(300);

    // TODO: Shut down all processes when the program exits OR a Ctrl+C is pressed.
    let futures = split_ranges
        .chunks(CONCURRENT_NATIVE_HOST_RUNNERS)
        .map(|chunk| {
            futures::future::join_all(chunk.iter().map(|range| async {
                let host_cli = data_fetcher
                    .get_host_cli_args(range.start, range.end, ProgramType::Multi)
                    .await
                    .unwrap();

                let data_dir = host_cli
                    .data_dir
                    .clone()
                    .expect("Data directory is not set.");

                fs::create_dir_all(&data_dir).unwrap();

                let res = run_native_host(&host_cli, NATIVE_HOST_TIMEOUT).await;
                if res.is_err() {
                    error!("Failed to run native host: {:?}", res.err().unwrap());
                    std::process::exit(1);
                }

                BatchHostCli {
                    host_cli,
                    start: range.start,
                    end: range.end,
                }
            }))
        });

    futures::future::join_all(futures)
        .await
        .into_iter()
        .flatten()
        .collect()
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
    data_fetcher: &SP1KonaDataFetcher,
) -> Vec<ExecutionStats> {
    host_clis
        .par_iter()
        .map(|r| {
            let sp1_stdin = get_proof_stdin(&r.host_cli).unwrap();

            let start_time = Instant::now();
            let (_, report) = prover.execute(MULTI_BLOCK_ELF, sp1_stdin).run().unwrap();
            let execution_duration = start_time.elapsed();
            block_on(get_execution_stats(
                data_fetcher,
                r.start,
                r.end,
                &report,
                execution_duration,
            ))
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
        csv_writer
            .serialize(stats)
            .expect("Failed to write execution stats to CSV.");
    }
    csv_writer.flush().expect("Failed to flush CSV writer.");

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    utils::setup_logger();

    let args = HostArgs::parse();
    let data_fetcher = SP1KonaDataFetcher::new();
    let l2_chain_id = data_fetcher.get_chain_id(ChainMode::L2).await?;
    let rollup_config = RollupConfig::from_l2_chain_id(l2_chain_id).unwrap();

    // TODO: Modify fetch_span_batch_ranges to start up the Docker container.
    let span_batch_ranges =
        fetch_span_batch_ranges(&data_fetcher, &args, l2_chain_id, &rollup_config).await?;
    let split_ranges = split_ranges(span_batch_ranges, l2_chain_id);

    info!(
        "The span batch ranges which will be executed: {:?}",
        split_ranges
    );

    let prover = ProverClient::new();
    let host_clis = run_native_data_generation(&data_fetcher, &split_ranges).await;

    let execution_stats = execute_blocks_parallel(&host_clis, &prover, &data_fetcher).await;
    write_execution_stats_to_csv(&execution_stats, l2_chain_id, &args)?;

    Ok(())
}
