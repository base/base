use std::time::{Duration, Instant};

use anyhow::{Ok, Result};
use kona_host::HostCli;
use op_succinct_host_utils::{
    fetcher::{BlockInfo, OPSuccinctDataFetcher},
    witnessgen::WitnessGenExecutor,
};
use sp1_sdk::{ExecutionReport, ProverClient, SP1Stdin};

pub const DEFAULT_RANGE: u64 = 5;
pub const TWO_WEEKS: Duration = Duration::from_secs(14 * 24 * 60 * 60);

pub const MULTI_BLOCK_ELF: &[u8] = include_bytes!("../../../elf/range-elf");

pub async fn generate_witness(host_cli: &HostCli) -> Result<Duration> {
    let start_time = Instant::now();

    // Start the server and native client.
    let mut witnessgen_executor = WitnessGenExecutor::default();
    witnessgen_executor.spawn_witnessgen(host_cli).await?;
    witnessgen_executor.flush().await?;

    let witness_generation_time_sec = start_time.elapsed();
    println!(
        "Witness Generation Duration: {:?}",
        witness_generation_time_sec.as_secs()
    );

    Ok(witness_generation_time_sec)
}

pub async fn execute_multi(
    prover: &ProverClient,
    data_fetcher: &OPSuccinctDataFetcher,
    sp1_stdin: SP1Stdin,
    l2_start_block: u64,
    l2_end_block: u64,
) -> Result<(Vec<BlockInfo>, ExecutionReport, Duration)> {
    let start_time = Instant::now();
    let (_, report) = prover
        .execute(MULTI_BLOCK_ELF, sp1_stdin.clone())
        .run()
        .unwrap();
    let execution_duration = start_time.elapsed();

    let block_data = data_fetcher
        .get_l2_block_data_range(l2_start_block, l2_end_block)
        .await?;

    Ok((block_data, report, execution_duration))
}
