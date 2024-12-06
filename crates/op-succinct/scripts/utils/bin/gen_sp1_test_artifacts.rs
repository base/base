use anyhow::Result;
use clap::Parser;
use futures::StreamExt;
use kona_host::HostCli;
use log::info;
use op_succinct_host_utils::{
    block_range::{get_validated_block_range, split_range_basic, SpanBatchRange},
    fetcher::{CacheMode, OPSuccinctDataFetcher, RunContext},
    get_proof_stdin,
    witnessgen::run_native_data_generation,
    ProgramType,
};
use op_succinct_scripts::HostExecutorArgs;
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use sp1_sdk::{utils, ProverClient, SP1Stdin};
use std::{
    fs::{self},
    path::PathBuf,
};

pub const MULTI_BLOCK_ELF: &[u8] = include_bytes!("../../../elf/range-elf");

/// Run the zkVM execution process for each split range in parallel. Get the SP1Stdin and the range
/// for each successful execution.
async fn execute_blocks_parallel(
    host_clis: &[HostCli],
    ranges: Vec<SpanBatchRange>,
    prover: &ProverClient,
) -> Vec<(SP1Stdin, SpanBatchRange)> {
    // Run the zkVM execution process for each split range in parallel and fill in the execution stats.
    let successful_ranges = host_clis
        .par_iter()
        .zip(ranges.par_iter())
        .map(|(host_cli, range)| {
            let sp1_stdin = get_proof_stdin(host_cli).unwrap();

            let result = prover.execute(MULTI_BLOCK_ELF, sp1_stdin.clone()).run();

            // If the execution fails, skip this block range and log the error.
            if let Some(err) = result.as_ref().err() {
                log::warn!(
                    "Failed to execute blocks {:?} - {:?} because of {:?}. Reduce your `batch-size` if you're running into OOM issues on SP1.",
                    range.start,
                    range.end,
                    err
                );
                return None;
            }

            Some((sp1_stdin.clone(), range.clone()))
        })
        .filter_map(|result| result)
        .collect();

    info!("Execution is complete.");

    successful_ranges
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = HostExecutorArgs::parse();

    dotenv::from_path(&args.env_file).ok();
    utils::setup_logger();

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config(RunContext::Dev).await?;
    let l2_chain_id = data_fetcher.get_l2_chain_id().await?;

    let (l2_start_block, l2_end_block) =
        get_validated_block_range(&data_fetcher, args.start, args.end, args.default_range).await?;

    let split_ranges = split_range_basic(l2_start_block, l2_end_block, args.batch_size);

    info!(
        "The span batch ranges which will be executed: {:?}",
        split_ranges
    );

    let prover = ProverClient::new();

    let cache_mode = if args.use_cache {
        CacheMode::KeepCache
    } else {
        CacheMode::DeleteCache
    };

    // Get the host CLIs in order, in parallel.
    let host_clis = futures::stream::iter(split_ranges.iter())
        .map(|range| async {
            data_fetcher
                .get_host_cli_args(range.start, range.end, ProgramType::Multi, cache_mode)
                .await
                .expect("Failed to get host CLI args")
        })
        .buffered(15)
        .collect::<Vec<_>>()
        .await;

    if !args.use_cache {
        // Get the host CLI args
        run_native_data_generation(&host_clis).await;
    }

    let successful_ranges = execute_blocks_parallel(&host_clis, split_ranges, &prover).await;

    // Now, write the successful ranges to /sp1-testing-suite-artifacts/op-succinct-chain-{l2_chain_id}-{start}-{end}
    // The folders should each have the MULTI_BLOCK_ELF as program.bin, and the serialized stdin should be
    // written to stdin.bin.
    let cargo_metadata = cargo_metadata::MetadataCommand::new().exec().unwrap();
    let root_dir = PathBuf::from(cargo_metadata.workspace_root).join("sp1-testing-suite-artifacts");

    let dir_name = root_dir.join(format!("op-succinct-chain-{}", l2_chain_id));
    info!("Writing artifacts to {:?}", dir_name);
    for (sp1_stdin, range) in successful_ranges {
        let program_dir = PathBuf::from(format!(
            "{}-{}-{}",
            dir_name.to_string_lossy(),
            range.start,
            range.end
        ));
        fs::create_dir_all(&program_dir)?;

        fs::write(program_dir.join("program.bin"), MULTI_BLOCK_ELF)?;
        fs::write(
            program_dir.join("stdin.bin"),
            bincode::serialize(&sp1_stdin).unwrap(),
        )?;
    }

    Ok(())
}
