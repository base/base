use std::time::{Duration, Instant};

use anyhow::{Ok, Result};
use op_succinct_host_utils::fetcher::{BlockInfo, OPSuccinctDataFetcher};
use op_succinct_proof_utils::get_range_elf_embedded;
use sp1_sdk::{ExecutionReport, ProverClient, SP1Stdin};

pub const DEFAULT_RANGE: u64 = 5;
pub const TWO_WEEKS: Duration = Duration::from_secs(14 * 24 * 60 * 60);
pub const ONE_HOUR: Duration = Duration::from_secs(60 * 60);

pub async fn execute_multi(
    data_fetcher: &OPSuccinctDataFetcher,
    sp1_stdin: SP1Stdin,
    l2_start_block: u64,
    l2_end_block: u64,
) -> Result<(Vec<BlockInfo>, ExecutionReport, Duration)> {
    let start_time = Instant::now();
    let prover = ProverClient::builder().mock().build();

    let (_, report) =
        prover.execute(get_range_elf_embedded(), &sp1_stdin).calculate_gas(true).run().unwrap();

    let execution_duration = start_time.elapsed();

    let block_data = data_fetcher.get_l2_block_data_range(l2_start_block, l2_end_block).await?;

    Ok((block_data, report, execution_duration))
}
