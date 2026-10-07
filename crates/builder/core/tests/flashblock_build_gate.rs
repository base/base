//! Event-volume budgets for the builder performance gate.
//!
//! Runs every [`FlashblockWorkload`] in the gate matrix through both production builders (the
//! flashblocks build loop and the native payload builder that serves Denim blocks) with
//! transaction events captured, and fails when a scenario emits more events of a type than
//! `etc/benchmarks/builder-gate-budgets.json` allows for that builder. Event types without a budget must not
//! be emitted at all, so a new per-candidate event fails here until someone budgets it.
//!
//! Event volume is the cheapest deterministic signal for per-candidate event cost on the builder
//! thread. A resting validity transaction is reconsidered on every flashblock and every wakeup,
//! so an event emitted per candidate or per park multiplies with the backlog. The budgets allow one
//! `BUILDER_DEFERRED` per transaction and reason per block. The instruction budgets for the same
//! scenarios live in `benches/flashblock_build_iai.rs`.

#![allow(missing_docs)]

use std::{collections::BTreeMap, path::PathBuf};

use base_builder_core::test_utils::{FlashblockWorkload, FlashblockWorkloadFixture};
use base_observability_events::TransactionEventCapture;
use rstest::rstest;
use serde_json::Value;

fn event_budgets(key: &str) -> BTreeMap<String, u64> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../etc/benchmarks/builder-gate-budgets.json");
    let budgets: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {path:?}: {err}")),
    )
    .expect("budget file is JSON");
    budgets["scenarios"][key]["events"]
        .as_object()
        .unwrap_or_else(|| panic!("no event budget for scenario {key}"))
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
fn block_build_stays_within_event_budget(
    #[case] scenario: &str,
    #[values("flashblocks", "native")] builder: &str,
) {
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
    let (included, outcome) = if native {
        let outcome = fixture.run_native_block().expect("native block builds");
        (outcome.included, format!("{outcome:?}"))
    } else {
        let outcome = fixture.run_block().expect("flashblock block builds");
        (outcome.included, format!("{outcome:?}"))
    };
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
    println!("{key}: build={elapsed:?} outcome={outcome} events={emitted:?}");
    assert_eq!(included, workload.expected_included(), "{key}: included");

    let budgets = event_budgets(&key);
    let over_budget = emitted
        .iter()
        .filter_map(|(event_type, &count)| {
            let budget = budgets.get(event_type).copied().unwrap_or(0);
            (count > budget).then(|| format!("{event_type}: emitted {count}, budget {budget}"))
        })
        .collect::<Vec<_>>();
    assert!(over_budget.is_empty(), "{key} exceeded its event budget: {over_budget:#?}");
}
