//! End-to-end check that a validity-predicate load-test workload drives the builder series the
//! snapshot benchmark collects.
//!
//! Runs the load tester's validity path (`base_sendRawTransactionValidity` with offset
//! `block_number` predicates) against a fresh devnet whose builder enforces resting predicates,
//! then scrapes the builder's Prometheus endpoint the way `base-bench snapshot` does and checks
//! that the flattened keys the benchmark allowlists are present with the expected shape.

use alloy_signer_local::PrivateKeySigner;
use base_builder_core::RestingPredicateMode;
use base_load_tests::{
    LoadTestExecutor, LoadTestRunHooks, LoadTestRunOptions, MetricsSummary, SubmitCohortLabel,
    TestConfig,
};
use base_system_tests::{
    ANVIL_ACCOUNT_1, DevnetConfig, PrometheusBlockCollector, PrometheusSnapshot,
    SystemTestStackBuilder,
};
use eyre::{Result, WrapErr};

/// Mirrors the `validity-resting` benchmark workload (every transaction is unsatisfied until two
/// blocks after it was prepared and expires thirty blocks later) with an always-true balance and
/// storage predicate in front, so account and slot reads are exercised too. Scaled down for a devnet.
const WORKLOAD: &str = r#"
transaction_submission_rpcs: ["http://placeholder"]
sender_count: 20
block_time: "2s"
in_flight_per_sender: 8
batch_size: 20
duration: "30s"
seed: 12345
funding_amount: "10000000000000000"
fresh_recipient_ratio: 0.0
skip_drain: true
validity:
  ratio: 1.0
  predicates:
    - {type: balance, address: sender, op: ">=", value: "0"}
    - {type: storage, address: sender, slot: {kind: fixed, value: "0x0"}, op: ">=", value: "0"}
    - {type: block_number, op: ">=", offset: "2"}
    - {type: block_number, op: "<", offset: "30"}
transactions:
  - {weight: 100, type: transfer}
"#;

#[tokio::test]
async fn validity_resting_workload_populates_benchmark_series() -> Result<()> {
    let devnet = DevnetConfig::standard();
    let chain_id = devnet.l2_chain_id;
    let stack = SystemTestStackBuilder::new()
        .with_devnet_config(devnet)
        .with_resting_predicate_mode(RestingPredicateMode::Enforce)
        .build()
        .await?;

    let result: Result<(MetricsSummary, PrometheusSnapshot)> = async {
        let mut test_config = TestConfig::from_yaml(WORKLOAD)?;
        test_config.transaction_submission_rpcs = vec![stack.l2_rpc_url()?];
        test_config.query_rpc = Some(stack.l2_client_rpc_url()?);
        test_config.flashblocks_ws =
            Some(stack.l2_stack().builder().flashblocks_url().parse().wrap_err("flashblocks url")?);
        test_config.chain_id = Some(chain_id);
        test_config.validate()?;
        let load_config = test_config.to_load_config(None)?;
        let funder = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)?;

        let output = LoadTestExecutor::run_prepared(
            test_config,
            load_config,
            funder,
            LoadTestRunOptions {
                continuous: false,
                install_signal_handler: false,
                skip_drain: true,
            },
            LoadTestRunHooks {
                display: None,
                before_cleanup: (|_: &MetricsSummary| {}) as fn(&MetricsSummary),
            },
        )
        .await?;
        if let Some(error) = output.run_error {
            eyre::bail!("load test run error: {error}");
        }

        let metrics_url = stack.l2_stack().builder().metrics_url()?;
        let body = reqwest::get(metrics_url).await?.error_for_status()?.text().await?;
        Ok((output.summary, PrometheusSnapshot::parse(&body)))
    }
    .await;

    let shutdown = stack.shutdown().await;
    let (summary, snapshot) = result?;
    shutdown?;

    // The load tester routed every sender through the validity endpoint and they landed.
    let validity = summary
        .by_cohort
        .iter()
        .find(|cohort| cohort.cohort == SubmitCohortLabel::ValidityPass)
        .expect("validity cohort summary");
    assert!(validity.confirmed > 0, "no validity transactions were confirmed: {summary:#?}");
    assert_eq!(validity.reverted, 0, "validity transactions reverted");
    assert!(
        summary
            .by_cohort
            .iter()
            .all(|c| c.cohort == SubmitCohortLabel::ValidityPass || c.confirmed == 0),
        "ratio 1.0 must leave the plain cohort empty: {:?}",
        summary.by_cohort
    );

    // Whole-run totals: deltas against an empty snapshot are the raw counters and histogram means.
    let totals = snapshot.delta(&PrometheusSnapshot::default());
    let validity_keys: Vec<_> = totals
        .iter()
        .filter(|(key, _)| {
            key.contains("validity")
                || key.contains("resting_predicate")
                || key.contains("predicate_")
        })
        .collect();
    println!("validity-related builder series after the run:");
    for (key, value) in &validity_keys {
        println!("  {key} = {value}");
    }

    // Series that this workload must drive, each allowlisted for the benchmark visualizer.
    let positive = [
        // Resting: unsatisfied on first evaluation, parked by the enforcing iterator, released at
        // the next block start because a block-number predicate blocked it, then matched.
        "reth_base_builder_validity_predicate_evaluations_total_outcome_not_satisfied",
        "reth_base_builder_resting_predicate_parked_total",
        "reth_base_builder_resting_predicate_carry_released_total_reason_block_number",
        "reth_base_builder_validity_predicate_evaluations_total_outcome_matched",
        // Evaluation volume, pool admission and inclusion.
        "reth_base_builder_validity_predicate_candidates_evaluated_per_build_avg",
        "reth_txpool_validity_admitted_outcome_added",
        "reth_base_builder_txs_included_per_block_flow_validity_avg",
    ];
    for key in positive {
        assert!(PrometheusBlockCollector::is_visualizer_metric(key), "{key} is not allowlisted");
        let value = totals.get(key).unwrap_or_else(|| panic!("{key} missing from scrape"));
        assert!(*value > 0.0, "{key} = {value}, expected > 0");
    }
    // Present (recorded every build) even when small.
    let present = [
        "reth_base_builder_validity_candidate_handling_duration_avg",
        "reth_base_builder_validity_predicate_eval_duration_per_block_avg",
        "reth_base_builder_validity_predicate_evaluation_coverage_per_build_avg",
        "reth_base_builder_predicate_accounts_loaded_total_avg",
        "reth_base_builder_predicate_accounts_loaded_unique_avg",
        "reth_base_builder_predicate_slots_loaded_total_avg",
        "reth_base_builder_predicate_slots_loaded_unique_avg",
        "reth_base_builder_resting_predicate_carried_avg",
        "reth_base_builder_predicate_bucket_depth_avg",
    ];
    for key in present {
        assert!(PrometheusBlockCollector::is_visualizer_metric(key), "{key} is not allowlisted");
        assert!(totals.contains_key(key), "{key} missing from scrape");
    }
    // Nothing expired within the 30-block window.
    assert_eq!(
        totals.get("reth_base_builder_validity_predicate_evaluations_total_outcome_expired"),
        None,
        "transactions expired before inclusion"
    );
    // Coverage is a fraction, and it drops below one only when the per-build evaluation budget
    // deferred candidates. A slow runner may legitimately trip the 10 ms default budget, so the
    // two series are checked for consistency rather than requiring full coverage.
    let coverage = totals["reth_base_builder_validity_predicate_evaluation_coverage_per_build_avg"];
    assert!((0.0..=1.0).contains(&coverage), "evaluation coverage {coverage} outside 0..=1");
    let deferred = totals["reth_base_builder_validity_predicate_candidates_deferred_per_build_avg"];
    let budget_exhausted = totals.contains_key(
        "reth_base_builder_validity_predicate_evaluations_total_outcome_budget_exhausted",
    );
    assert_eq!(
        coverage < 1.0,
        deferred > 0.0 && budget_exhausted,
        "coverage {coverage}, deferred {deferred}, budget_exhausted {budget_exhausted} disagree"
    );

    Ok(())
}
