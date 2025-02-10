use anyhow::Result;
use common::post_to_github_pr;
use op_succinct_host_utils::{
    block_range::get_rolling_block_range,
    fetcher::{CacheMode, OPSuccinctDataFetcher, RunContext},
    get_proof_stdin, start_server_and_native_client,
    stats::{ExecutionStats, MarkdownExecutionStats},
    ProgramType,
};
use op_succinct_prove::{execute_multi, DEFAULT_RANGE, ONE_HOUR};

mod common;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn execute_batch() -> Result<()> {
    dotenv::dotenv()?;

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config(RunContext::Dev).await?;

    // Take the latest blocks
    let (l2_start_block, l2_end_block) =
        get_rolling_block_range(&data_fetcher, ONE_HOUR, DEFAULT_RANGE).await?;

    let host_cli = data_fetcher
        .get_host_cli_args(
            l2_start_block,
            l2_end_block,
            ProgramType::Multi,
            CacheMode::DeleteCache,
        )
        .await?;

    let oracle = start_server_and_native_client(host_cli.clone()).await?;

    // Get the stdin for the block.
    let sp1_stdin = get_proof_stdin(oracle)?;

    let (block_data, report, execution_duration) =
        execute_multi(&data_fetcher, sp1_stdin, l2_start_block, l2_end_block).await?;

    let l1_block_number = data_fetcher
        .get_l1_header(host_cli.l1_head.into())
        .await
        .unwrap()
        .number;
    let stats = ExecutionStats::new(
        l1_block_number,
        &block_data,
        &report,
        0,
        execution_duration.as_secs(),
    );

    println!("Execution Stats: \n{:?}", stats.to_string());

    if std::env::var("POST_TO_GITHUB")
        .ok()
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or_default()
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
