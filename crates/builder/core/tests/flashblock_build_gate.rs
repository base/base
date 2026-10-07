//! Event-volume budgets for the builder performance gate.
//!
//! Runs every [`FlashblockWorkload`] in the gate matrix through the production flashblock build
//! loop with transaction events captured, and fails when a scenario emits more events of a type
//! than `etc/benchmarks/builder-gate-budgets.json` allows. Event types without a budget must not
//! be emitted at all, so a new per-candidate event fails here until someone budgets it.
//!
//! Event volume is the cheapest deterministic signal for the 2026-10-06 regression: before
//! #5623 the builder emitted `BUILDER_CONSIDERED` for every candidate and `BUILDER_DEFERRED` on
//! every park, so the incident-scale backlog emitted ~10x its size per block. The instruction
//! budgets for the same scenarios live in `benches/flashblock_build_iai.rs`.

#![allow(missing_docs)]

use std::{collections::BTreeMap, path::PathBuf};

use base_builder_core::test_utils::{FlashblockWorkload, FlashblockWorkloadFixture};
use base_observability_events::TransactionEventCapture;
use rstest::rstest;
use serde_json::Value;

fn event_budgets(scenario: &str) -> BTreeMap<String, u64> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../etc/benchmarks/builder-gate-budgets.json");
    let budgets: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {path:?}: {err}")),
    )
    .expect("budget file is JSON");
    budgets["scenarios"][scenario]["events"]
        .as_object()
        .unwrap_or_else(|| panic!("no event budget for scenario {scenario}"))
        .iter()
        .map(|(event_type, max)| (event_type.clone(), max.as_u64().expect("integer budget")))
        .collect()
}

#[rstest]
#[case::transfers("transfers")]
#[case::resting_backlog("resting_backlog")]
#[case::resting_backlog_multi_predicate("resting_backlog_multi_predicate")]
#[case::resting_backlog_shared_state("resting_backlog_shared_state")]
#[case::wake_rescan("wake_rescan")]
#[case::backlog_growth("backlog_growth")]
#[case::congested("congested")]
#[case::satisfied_validity("satisfied_validity")]
fn flashblock_build_stays_within_event_budget(#[case] scenario: &str) {
    let workload = FlashblockWorkload::by_name(scenario).expect("scenario is in the matrix");
    let mut fixture = FlashblockWorkloadFixture::new(workload);
    let capture = TransactionEventCapture::install();

    let started = std::time::Instant::now();
    let outcome = fixture.run_block().expect("in-memory block builds");
    let elapsed = started.elapsed();

    let mut emitted = BTreeMap::<String, u64>::new();
    let events = capture.events();
    if std::env::var_os("BUILDER_GATE_DUMP_EVENTS").is_some() {
        for event in events.iter().take(5) {
            println!("{}", serde_json::to_string(event).unwrap());
        }
    }
    for event in events {
        *emitted.entry(event.event_type.to_string()).or_default() += 1;
    }
    println!("{scenario}: build={elapsed:?} outcome={outcome:?} events={emitted:?}");
    assert_eq!(outcome.included, workload.expected_included(), "{scenario}: included");

    let budgets = event_budgets(scenario);
    let over_budget = emitted
        .iter()
        .filter_map(|(event_type, &count)| {
            let budget = budgets.get(event_type).copied().unwrap_or(0);
            (count > budget).then(|| format!("{event_type}: emitted {count}, budget {budget}"))
        })
        .collect::<Vec<_>>();
    assert!(over_budget.is_empty(), "{scenario} exceeded its event budget: {over_budget:#?}");
}
