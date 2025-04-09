use anyhow::Result;
use std::{fmt::Write as _, fs::File, sync::Arc};

use common::post_to_github_pr;
use op_succinct_host_utils::{
    fetcher::OPSuccinctDataFetcher,
    get_proof_stdin,
    hosts::{default::SingleChainOPSuccinctHost, OPSuccinctHost},
    stats::{ExecutionStats, MarkdownExecutionStats},
};
use op_succinct_prove::execute_multi;

mod common;

fn create_diff_report(
    base: &ExecutionStats,
    current: &ExecutionStats,
    range: (u64, u64),
) -> String {
    let mut report = String::new();
    writeln!(report, "## Performance Comparison\n").unwrap();
    writeln!(report, "Range {}~{}\n", range.0, range.1).unwrap();
    writeln!(
        report,
        "| {:<30} | {:<25} | {:<25} | {:<10} |",
        "Metric", "Base Branch", "Current PR", "Diff (%)"
    )
    .unwrap();
    writeln!(report, "|--------------------------------|---------------------------|---------------------------|------------|").unwrap();

    let diff_percentage = |base: u64, current: u64| -> f64 {
        if base == 0 {
            return 0.0;
        }
        ((current as f64 - base as f64) / base as f64) * 100.0
    };

    let write_metric = |report: &mut String, name: &str, base_val: u64, current_val: u64| {
        let diff = diff_percentage(base_val, current_val);
        writeln!(
            report,
            "| {:<30} | {:<25} | {:<25} | {:>9.2}% |",
            name,
            base_val.to_string(),
            current_val.to_string(),
            diff
        )
        .unwrap();
    };

    // Add key metrics with their comparisons
    write_metric(
        &mut report,
        "Total Instructions",
        base.total_instruction_count,
        current.total_instruction_count,
    );
    write_metric(
        &mut report,
        "Oracle Verify Cycles",
        base.oracle_verify_instruction_count,
        current.oracle_verify_instruction_count,
    );
    write_metric(
        &mut report,
        "Derivation Cycles",
        base.derivation_instruction_count,
        current.derivation_instruction_count,
    );
    write_metric(
        &mut report,
        "Block Execution Cycles",
        base.block_execution_instruction_count,
        current.block_execution_instruction_count,
    );
    write_metric(
        &mut report,
        "Blob Verification Cycles",
        base.blob_verification_instruction_count,
        current.blob_verification_instruction_count,
    );
    write_metric(
        &mut report,
        "Total SP1 Gas",
        base.total_sp1_gas,
        current.total_sp1_gas,
    );
    write_metric(
        &mut report,
        "Cycles per Block",
        base.cycles_per_block,
        current.cycles_per_block,
    );
    write_metric(
        &mut report,
        "Cycles per Transaction",
        base.cycles_per_transaction,
        current.cycles_per_transaction,
    );
    write_metric(
        &mut report,
        "BN Pair Cycles",
        base.bn_pair_cycles,
        current.bn_pair_cycles,
    );
    write_metric(
        &mut report,
        "BN Add Cycles",
        base.bn_add_cycles,
        current.bn_add_cycles,
    );
    write_metric(
        &mut report,
        "BN Mul Cycles",
        base.bn_mul_cycles,
        current.bn_mul_cycles,
    );
    write_metric(
        &mut report,
        "KZG Eval Cycles",
        base.kzg_eval_cycles,
        current.kzg_eval_cycles,
    );
    write_metric(
        &mut report,
        "EC Recover Cycles",
        base.ec_recover_cycles,
        current.ec_recover_cycles,
    );
    write_metric(
        &mut report,
        "P256 Verify Cycles",
        base.p256_verify_cycles,
        current.p256_verify_cycles,
    );

    report
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_cycle_count_diff() -> Result<()> {
    dotenv::dotenv()?;

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

    let host = SingleChainOPSuccinctHost {
        fetcher: Arc::new(data_fetcher.clone()),
    };

    let base_stats =
        serde_json::from_reader::<_, ExecutionStats>(File::open("base_cycle_stats.json")?)?;
    let l2_start_block = base_stats.batch_start;
    let l2_end_block = base_stats.batch_end;

    let host_args = host
        .fetch(l2_start_block, l2_end_block, None, Some(false))
        .await?;

    let oracle = host.run(&host_args).await?;
    let sp1_stdin = get_proof_stdin(oracle)?;
    let (block_data, report, execution_duration) =
        execute_multi(&data_fetcher, sp1_stdin, l2_start_block, l2_end_block).await?;

    let l1_block_number = data_fetcher
        .get_l1_header(host_args.l1_head.into())
        .await
        .unwrap()
        .number;
    let new_stats = ExecutionStats::new(
        l1_block_number,
        &block_data,
        &report,
        0,
        execution_duration.as_secs(),
    );

    println!(
        "Execution Stats:\n{}",
        MarkdownExecutionStats::new(new_stats.clone())
    );

    let report = create_diff_report(&base_stats, &new_stats, (l2_start_block, l2_end_block));

    // Update base_cycle_stats.json with new stats.
    let mut file = File::create("base_cycle_stats.json")?;
    serde_json::to_writer_pretty(&mut file, &new_stats)?;

    // Commit the changes to base_cycle_stats.json
    let git_add = std::process::Command::new("git")
        .arg("add")
        .arg("base_cycle_stats.json")
        .output()?;
    if !git_add.status.success() {
        eprintln!(
            "Failed to git add base_cycle_stats.json: {}",
            String::from_utf8_lossy(&git_add.stderr)
        );
    }

    let git_commit = std::process::Command::new("git")
        .arg("commit")
        .arg("-m")
        .arg(format!(
            "chore: update base cycle stats for blocks {}~{}",
            l2_start_block, l2_end_block
        ))
        .output()?;
    if !git_commit.status.success() {
        eprintln!(
            "Failed to git commit base_cycle_stats.json: {}",
            String::from_utf8_lossy(&git_commit.stderr)
        );
    }

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
            post_to_github_pr(&owner, &repo, &pr_number, &token, &report)
                .await
                .unwrap();
        }
    }

    Ok(())
}
