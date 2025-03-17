use anyhow::Result;
use clap::Parser;
use futures::StreamExt;
use log::info;
use op_succinct_host_utils::{
    block_range::{get_validated_block_range, split_range_basic},
    fetcher::OPSuccinctDataFetcher,
    get_proof_stdin,
    hosts::{default::SingleChainOPSuccinctHost, OPSuccinctHost},
    RANGE_ELF_EMBEDDED,
};
use op_succinct_scripts::HostExecutorArgs;
use sp1_sdk::utils;
use std::{
    fs::{self},
    path::PathBuf,
    sync::Arc,
};

#[tokio::main]
async fn main() -> Result<()> {
    let args = HostExecutorArgs::parse();

    dotenv::from_path(&args.env_file).ok();
    utils::setup_logger();

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;
    let l2_chain_id = data_fetcher.get_l2_chain_id().await?;

    let (l2_start_block, l2_end_block) =
        get_validated_block_range(&data_fetcher, args.start, args.end, args.default_range).await?;

    let split_ranges = split_range_basic(l2_start_block, l2_end_block, args.batch_size);

    info!(
        "The span batch ranges which will be executed: {:?}",
        split_ranges
    );

    // Get the host CLIs in order, in parallel.
    let host = Arc::new(SingleChainOPSuccinctHost {
        fetcher: Arc::new(data_fetcher),
    });
    let host_args = futures::stream::iter(split_ranges.iter())
        .map(|range| async {
            host.fetch(range.start, range.end, None, Some(args.safe_db_fallback))
                .await
                .expect("Failed to get host CLI args")
        })
        .buffered(15)
        .collect::<Vec<_>>()
        .await;

    let mut successful_ranges = Vec::new();
    for (range, host_args) in split_ranges.iter().zip(host_args.iter()) {
        let oracle = host.run(host_args).await.unwrap();
        let sp1_stdin = get_proof_stdin(oracle).unwrap();
        successful_ranges.push((sp1_stdin, range.clone()));
    }

    // Now, write the successful ranges to /sp1-testing-suite-artifacts/op-succinct-chain-{l2_chain_id}-{start}-{end}
    // The folders should each have the RANGE_ELF_EMBEDDED as program.bin, and the serialized stdin should be
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

        fs::write(program_dir.join("program.bin"), RANGE_ELF_EMBEDDED)?;
        fs::write(
            program_dir.join("stdin.bin"),
            bincode::serialize(&sp1_stdin).unwrap(),
        )?;
    }

    Ok(())
}
