use std::{fmt::Write as _, fs::File, sync::Arc};

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
use op_succinct_prove::{execute_multi, DEFAULT_RANGE};

mod common;

fn elf_label() -> &'static str {
    cfg_if::cfg_if! {
        if #[cfg(feature = "celestia")] {
            "celestia-range-elf-embedded"
        } else if #[cfg(feature = "eigenda")] {
            "eigenda-range-elf-embedded"
        } else {
            "range-elf-embedded"
        }
    }
}

fn create_diff_report(base: &ExecutionStats, current: &ExecutionStats) -> String {
    let mut report = String::new();
    writeln!(report, "## Performance Comparison (ELF: {})\n", elf_label()).unwrap();
    writeln!(report, "Range {}~{}\n", base.batch_start, base.batch_end).unwrap();
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

    // Add key metrics with their comparisons.
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
    write_metric(&mut report, "Total SP1 Gas", base.total_sp1_gas, current.total_sp1_gas);
    write_metric(&mut report, "Cycles per Block", base.cycles_per_block, current.cycles_per_block);
    write_metric(
        &mut report,
        "Cycles per Transaction",
        base.cycles_per_transaction,
        current.cycles_per_transaction,
    );
    write_metric(&mut report, "BN Pair Cycles", base.bn_pair_cycles, current.bn_pair_cycles);
    write_metric(&mut report, "BN Add Cycles", base.bn_add_cycles, current.bn_add_cycles);
    write_metric(&mut report, "BN Mul Cycles", base.bn_mul_cycles, current.bn_mul_cycles);
    write_metric(&mut report, "KZG Eval Cycles", base.kzg_eval_cycles, current.kzg_eval_cycles);
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

    let provider = rustls::crypto::ring::default_provider();
    provider
        .install_default()
        .map_err(|e| anyhow::anyhow!("Failed to install default provider: {:?}", e))?;

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

    let host = initialize_host(Arc::new(data_fetcher.clone()));
    let (l2_start_block, l2_end_block) = match std::env::var("NEW_BRANCH")
        .expect("NEW_BRANCH must be set")
        .parse::<bool>()
        .unwrap_or_default()
    {
        true => get_rolling_block_range(host.as_ref(), &data_fetcher, DEFAULT_RANGE).await?,
        false => {
            let base_stats =
                serde_json::from_reader::<_, ExecutionStats>(File::open("new_cycle_stats.json")?)?;
            (base_stats.batch_start, base_stats.batch_end)
        }
    };

    let host_args = host.fetch(l2_start_block, l2_end_block, None, false).await?;

    let witness_data = host.run(&host_args).await?;
    let sp1_stdin = host.witness_generator().get_sp1_stdin(witness_data)?;
    let (block_data, report, execution_duration) =
        execute_multi(&data_fetcher, sp1_stdin, l2_start_block, l2_end_block).await?;

    let new_stats = ExecutionStats::new(0, &block_data, &report, 0, execution_duration.as_secs());

    println!("Execution Stats:\n{}", MarkdownExecutionStats::new(new_stats.clone()));
    let mut file = match std::env::var("NEW_BRANCH")
        .expect("NEW_BRANCH must be set")
        .parse::<bool>()
        .unwrap_or_default()
    {
        true => File::create("new_cycle_stats.json")?,
        false => File::create("old_cycle_stats.json")?,
    };
    serde_json::to_writer_pretty(&mut file, &new_stats)?;

    Ok(())
}

#[tokio::test]
async fn test_post_to_github() -> Result<()> {
    let old_stats =
        serde_json::from_reader::<_, ExecutionStats>(File::open("old_cycle_stats.json")?)?;
    let new_stats =
        serde_json::from_reader::<_, ExecutionStats>(File::open("new_cycle_stats.json")?)?;
    let report = create_diff_report(&old_stats, &new_stats);

    if std::env::var("POST_TO_GITHUB").ok().and_then(|v| v.parse::<bool>().ok()).unwrap_or_default()
    {
        if let (Ok(owner), Ok(repo), Ok(pr_number), Ok(token)) = (
            std::env::var("REPO_OWNER"),
            std::env::var("REPO_NAME"),
            std::env::var("PR_NUMBER"),
            std::env::var("GITHUB_TOKEN"),
        ) {
            post_to_github_pr(&owner, &repo, &pr_number, &token, &report).await.unwrap();
        }
    }

    Ok(())
}
