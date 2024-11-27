use anyhow::Result;
use common::post_to_github_pr;
use op_succinct_host_utils::{
    block_range::get_rolling_block_range,
    fetcher::{CacheMode, OPSuccinctDataFetcher},
    get_proof_stdin,
    stats::{ExecutionStats, MarkdownExecutionStats},
    ProgramType,
};
use op_succinct_prove::{execute_multi, generate_witness, DEFAULT_RANGE, ONE_HOUR};
use sp1_sdk::ProverClient;

mod common;

#[tokio::test]
async fn execute_batch() -> Result<()> {
    dotenv::dotenv()?;

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

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

    let witness_generation_time_sec = generate_witness(&host_cli).await?;

    // Get the stdin for the block.
    let sp1_stdin = get_proof_stdin(&host_cli)?;

    let prover = ProverClient::new();

    let (block_data, report, execution_duration) = execute_multi(
        &prover,
        &data_fetcher,
        sp1_stdin,
        l2_start_block,
        l2_end_block,
    )
    .await?;

    let stats = ExecutionStats::new(
        &block_data,
        &report,
        witness_generation_time_sec.as_secs(),
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
