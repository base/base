//! End-to-end check that a validity-predicate load-test workload drives the builder series the
//! snapshot benchmark collects.
//!
//! Runs the load tester's validity path (`base_sendRawTransactionValidity` with slot-parity and
//! offset `block_number` predicates) against a fresh devnet whose builder enforces resting
//! predicates, then scrapes the builder's Prometheus endpoint the way `base-bench snapshot` does
//! and checks that the flattened keys the benchmark allowlists are present with the expected shape.
//!
//! The devnet activates Denim at genesis: the native builder only parks resting transactions
//! while one-shot payload building is active, which is a Denim feature, so a pre-Denim devnet
//! would never populate the resting series.

use std::time::Duration;

use alloy_signer_local::PrivateKeySigner;
use base_load_tests::{
    LoadTestExecutor, LoadTestRunHooks, LoadTestRunOptions, MetricsSummary, SubmitCohortLabel,
};
use base_system_tests::{
    ANVIL_ACCOUNT_1, DevnetConfig, PendingFixtures, PrometheusBlockCollector, PrometheusSnapshot,
    SystemTestStackBuilder,
};
use eyre::Result;
use tokio_util::task::AbortOnDropHandle;

/// How often the mutator flips the `DoubleCounter` slot the workload's parity predicate reads.
///
/// Short enough that a parked sender's in-flight transactions are released again within the run;
/// long enough that they first rest across several of the devnet's 200 ms builds, which is what
/// the parked and carried series count.
const MUTATOR_INTERVAL: Duration = Duration::from_millis(1000);

/// Mirrors the `validity-resting` benchmark workload (transactions gated on the parity of a
/// mutator-flipped `DoubleCounter` slot, so each sender alternates between matching and resting,
/// and expires a minute after it was prepared) with an always-true balance predicate in front, so
/// account reads are exercised too. Scaled down for a devnet; the measured transactions stay
/// plain transfers because the resting series depend on predicate evaluation, not the payload.
///
/// The workload adds the `>=` block-number rest window the real workload does not have, so the
/// scaled-down run also drives the block-number carry-release reason the visualizer allowlists.
///
/// The block time must match the devnet the stack builds: activating Denim at genesis puts the
/// chain on its 200 ms cadence, and the load runner's block watcher paces its confirmation scans
/// by this interval, so a workload slower than the chain schedules its scans minutes out and
/// observes no inclusions. The block-number offsets are counted in blocks, so they are stated in
/// 200 ms units: ten blocks until the resting predicate is satisfied, three hundred (the 60 s
/// ingress expiry window) until it expires.
const WORKLOAD: &str = r#"
transaction_submission_rpcs: ["http://placeholder"]
sender_count: 20
block_time: "200ms"
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
    - {type: storage, address: "__DOUBLE_COUNTER__", slot: {kind: fixed, value: "0x0"}, mask: "0x1", op: "=", value: sender_parity}
    - {type: block_number, op: ">=", offset: "10"}
    - {type: block_number, op: "<", offset: "300"}
transactions:
  - {weight: 100, type: transfer}
"#;

#[tokio::test]
async fn validity_resting_workload_populates_benchmark_series() -> Result<()> {
    let devnet = DevnetConfig::standard();
    let chain_id = devnet.l2_chain_id;
    let stack = SystemTestStackBuilder::new()
        .with_devnet_config(devnet)
        .with_base_denim_activation_block(0)
        .build()
        .await?;

    let result: Result<(MetricsSummary, PrometheusSnapshot, Option<AbortOnDropHandle<()>>)> =
        async {
            let workload = tempfile::NamedTempFile::new()?;
            std::fs::write(workload.path(), WORKLOAD)?;
            let (_, pending) = PendingFixtures::load(workload.path())?;
            let pending = pending.expect("placeholder workload must leave fixtures pending");

            let funder = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)?;
            let builder_rpc = stack.l2_rpc_url()?;
            // Fixtures deploy before the load tester funds its senders, so the funder's nonce stream is
            // used by one party at a time; the mutator then runs on its own funded key until its handle
            // drops, which the caller does once the run is over.
            let (mut test_config, mutator) = {
                let (resolved, deployed) = pending.deploy(&builder_rpc, &funder).await?;
                let mutator = deployed.start_mutator(&builder_rpc, MUTATOR_INTERVAL);
                (resolved, mutator)
            };

            test_config.transaction_submission_rpcs = vec![stack.l2_rpc_url()?];
            test_config.query_rpc = Some(stack.l2_client_rpc_url()?);
            test_config.chain_id = Some(chain_id);
            test_config.validate()?;
            let load_config = test_config.to_load_config(None)?;

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
            Ok((output.summary, PrometheusSnapshot::parse(&body), mutator))
        }
        .await;

    // Stop the mutator before teardown so its transactions do not race the stack's shutdown. A
    // failed body already dropped the handle, aborting the mutator; the stack still tears down.
    let (summary, snapshot, mutator) = match result {
        Ok(parts) => parts,
        // Report the body's error, not a teardown failure on top of it.
        Err(error) => {
            let _ = stack.shutdown().await;
            return Err(error);
        }
    };
    drop(mutator);
    let shutdown = stack.shutdown().await;
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
        // Resting: a sender whose parity bit does not match the mutator-flipped slot is unsatisfied
        // on first evaluation and parked by the enforcing iterator, carried into later builds where
        // the pool iterator holds it at the source until the next flip releases it by state, then
        // matched. A transaction still inside its block-number rest window is instead released at
        // the next block start, driving the block-number carry-release reason.
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
    // Nothing expired within the 300-block window.
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
