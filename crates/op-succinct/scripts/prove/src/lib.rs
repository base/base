use std::time::{Duration, Instant};

use anyhow::{Ok, Result};
use op_succinct_host_utils::fetcher::{BlockInfo, OPSuccinctDataFetcher};
use op_succinct_proof_utils::get_range_elf_embedded;
use sp1_sdk::{
    blocking::{CpuProver, Prover},
    Elf, ExecutionReport, SP1Stdin,
};

pub const TWO_WEEKS: Duration = Duration::from_secs(14 * 24 * 60 * 60);

pub async fn execute_multi(
    data_fetcher: &OPSuccinctDataFetcher,
    sp1_stdin: SP1Stdin,
    l2_start_block: u64,
    l2_end_block: u64,
) -> Result<(Vec<BlockInfo>, ExecutionReport, Duration)> {
    let start_time = Instant::now();

    // CpuProver::new() creates its own tokio runtime internally, so it must be constructed
    // and run outside the main tokio runtime context via spawn_blocking.
    let (_, report) = tokio::task::spawn_blocking(move || {
        let prover = CpuProver::new();
        prover
            .execute(Elf::Static(get_range_elf_embedded()), sp1_stdin)
            .calculate_gas(true)
            .deferred_proof_verification(false)
            .run()
    })
    .await??;

    let execution_duration = start_time.elapsed();

    let block_data = data_fetcher.get_l2_block_data_range(l2_start_block, l2_end_block).await?;

    Ok((block_data, report, execution_duration))
}
