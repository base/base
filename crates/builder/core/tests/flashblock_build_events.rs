//! Transaction-event volume of the builder benchmark workloads.
//!
//! Runs every [`FlashblockWorkload`] in the benchmark matrix through both production builders
//! (the flashblocks build loop and the native payload builder that serves Denim blocks), and
//! the flashblocks-only scenarios through the flashblocks loop, with transaction events
//! captured. Each case checks that the block includes the workload's expected
//! transactions, then prints one `BUILDER_BENCH` line with the per-type event counts and the
//! number of deferrals. `.depot/workflows/bench-builder.yml` runs this on the base and head
//! commits of a PR and reports the differences next to the instruction counts from
//! `benches/flashblock_build_iai.rs`.
//!
//! Event volume matters because a resting validity transaction is reconsidered on every
//! flashblock and every wakeup, so an event emitted per candidate or per park multiplies with
//! the backlog.

use std::collections::BTreeMap;

use base_builder_core::test_utils::{FlashblockWorkload, FlashblockWorkloadFixture};
use base_observability_events::TransactionEventCapture;
use rstest::rstest;
use serde_json::json;

#[rstest]
#[case::transfers("transfers")]
#[case::resting_backlog("resting_backlog")]
#[case::resting_backlog_multi_predicate("resting_backlog_multi_predicate")]
#[case::resting_backlog_shared_state("resting_backlog_shared_state")]
#[case::wake_rescan("wake_rescan")]
#[case::backlog_growth("backlog_growth")]
#[case::congested("congested")]
#[case::satisfied_validity("satisfied_validity")]
fn block_build_reports_event_volume(
    #[case] scenario: &str,
    #[values("flashblocks", "native")] builder: &str,
) {
    report_event_volume(scenario, builder);
}

/// Scenarios that set a flashblocks iterator mode the native builder does not have.
#[rstest]
#[case::resting_backlog_enforce("resting_backlog_enforce")]
#[case::wake_rescan_enforce("wake_rescan_enforce")]
#[case::backlog_growth_enforce("backlog_growth_enforce")]
fn flashblocks_only_build_reports_event_volume(#[case] scenario: &str) {
    report_event_volume(scenario, "flashblocks");
}

/// Builds one block of `scenario` on `builder`, checks inclusion, and prints its event counts.
fn report_event_volume(scenario: &str, builder: &str) {
    let workload = FlashblockWorkload::by_name(scenario).expect("scenario is in the matrix");
    let key = format!("{builder}/{scenario}");
    let native = builder == "native";
    let mut fixture = if native {
        FlashblockWorkloadFixture::new_native(workload)
    } else {
        FlashblockWorkloadFixture::new(workload)
    };
    let capture = TransactionEventCapture::install();

    let started = std::time::Instant::now();
    let (included, flashblock_deferrals) = if native {
        (fixture.run_native_block().expect("native block builds").included, None)
    } else {
        let outcome = fixture.run_block().expect("flashblock block builds");
        (outcome.included, Some(outcome.deferred))
    };
    let elapsed = started.elapsed();

    let mut events = BTreeMap::<String, u64>::new();
    for event in capture.events() {
        *events.entry(event.event_type.to_string()).or_default() += 1;
    }
    assert_eq!(included, workload.expected_included(), "{key}: included");

    // The native builder parks each transaction once per block and reports every park as
    // `BUILDER_DEFERRED`; the flashblocks loop re-parks on every flashblock and counts each.
    let deferrals = flashblock_deferrals
        .unwrap_or_else(|| events.get("BUILDER_DEFERRED").copied().unwrap_or_default());
    println!(
        "BUILDER_BENCH {}",
        json!({
            "key": key,
            "included": included,
            "deferrals": deferrals,
            "events": events,
            "wall_ms": elapsed.as_secs_f64() * 1e3,
        })
    );
}
