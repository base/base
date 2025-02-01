use std::time::{Duration, Instant};

use anyhow::{Ok, Result};
use op_succinct_host_utils::fetcher::{BlockInfo, OPSuccinctDataFetcher};
use sp1_sdk::{ExecutionReport, ProverClient, SP1Stdin};

pub const DEFAULT_RANGE: u64 = 5;
pub const TWO_WEEKS: Duration = Duration::from_secs(14 * 24 * 60 * 60);
pub const ONE_HOUR: Duration = Duration::from_secs(60 * 60);

pub const AGG_ELF: &[u8] = include_bytes!("../../../elf/aggregation-elf");
pub const RANGE_ELF: &[u8] = include_bytes!("../../../elf/range-elf");

pub async fn execute_multi(
    data_fetcher: &OPSuccinctDataFetcher,
    sp1_stdin: SP1Stdin,
    l2_start_block: u64,
    l2_end_block: u64,
) -> Result<(Vec<BlockInfo>, ExecutionReport, Duration)> {
    let start_time = Instant::now();
    let prover = ProverClient::builder().mock().build();
    let (_, report) = prover.execute(RANGE_ELF, &sp1_stdin).run().unwrap();
    let execution_duration = start_time.elapsed();

    let block_data = data_fetcher
        .get_l2_block_data_range(l2_start_block, l2_end_block)
        .await?;

    Ok((block_data, report, execution_duration))
}
