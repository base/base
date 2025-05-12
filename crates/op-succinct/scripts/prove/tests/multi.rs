use std::sync::Arc;

use anyhow::Result;
use common::post_to_github_pr;
use op_succinct_host_utils::{
    block_range::get_rolling_block_range,
    fetcher::OPSuccinctDataFetcher,
    host::OPSuccinctHost,
    stats::{ExecutionStats, MarkdownExecutionStats},
    witness_generation::WitnessGenerator,
};
use op_succinct_proof_utils::initialize_host;
use op_succinct_prove::{execute_multi, DEFAULT_RANGE, ONE_HOUR};

mod common;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn execute_batch() -> Result<()> {
    dotenv::dotenv()?;

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

    // Take the latest blocks
    let (l2_start_block, l2_end_block) =
        get_rolling_block_range(&data_fetcher, ONE_HOUR, DEFAULT_RANGE).await?;

    let host = initialize_host(Arc::new(data_fetcher.clone()));

    let host_args = host.fetch(l2_start_block, l2_end_block, None, Some(false)).await?;

    let witness_data = host.run(&host_args).await?;

    // Get the stdin for the block.
    let sp1_stdin = host.witness_generator().get_sp1_stdin(witness_data)?;

    let (block_data, report, execution_duration) =
        execute_multi(&data_fetcher, sp1_stdin, l2_start_block, l2_end_block).await?;

    let stats = ExecutionStats::new(0, &block_data, &report, 0, execution_duration.as_secs());

    println!("Execution Stats: \n{:?}", stats.to_string());

    if std::env::var("POST_TO_GITHUB").ok().and_then(|v| v.parse::<bool>().ok()).unwrap_or_default()
    {
        if let (Ok(owner), Ok(repo), Ok(pr_number), Ok(token)) = (
            std::env::var("REPO_OWNER"),
            std::env::var("REPO_NAME"),
            std::env::var("PR_NUMBER"),
            std::env::var("GITHUB_TOKEN"),
        ) {
            post_to_github_pr(
                &owner,
                &repo,
                &pr_number,
                &token,
                &MarkdownExecutionStats::new(stats).to_string(),
            )
            .await
            .unwrap();
        }
    }

    Ok(())
}
