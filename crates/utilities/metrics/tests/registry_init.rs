//! Integration test for static metric initializer registration.

use metrics_util::{
    MetricKind,
    debugging::{DebugValue, DebuggingRecorder},
};
use ordered_float::OrderedFloat;

type SnapEntry =
    (metrics_util::CompositeKey, Option<metrics::Unit>, Option<metrics::SharedString>, DebugValue);

fn find_metric<'a>(snap: &'a [SnapEntry], kind: MetricKind, name: &str) -> Option<&'a DebugValue> {
    snap.iter()
        .find(|(ck, _, _, _)| ck.kind() == kind && ck.key().name() == name)
        .map(|(_, _, _, v)| v)
}

fn find_metric_labeled<'a>(
    snap: &'a [SnapEntry],
    kind: MetricKind,
    name: &str,
    labels: &[(&str, &str)],
) -> Option<&'a DebugValue> {
    snap.iter()
        .find(|(ck, _, _, _)| {
            ck.kind() == kind
                && ck.key().name() == name
                && labels
                    .iter()
                    .all(|(k, v)| ck.key().labels().any(|l| l.key() == *k && l.value() == *v))
        })
        .map(|(_, _, _, v)| v)
}

base_metrics::define_metrics! {
    registry_test,
    struct = RegistryMetrics,

    #[describe("Auto-initialized counter")]
    ready_total: counter,

    #[describe("Auto-initialized gauge")]
    #[label(name = "status", default = ["idle", "busy"])]
    worker_state: gauge,
}

#[test]
fn initialize_registered_metrics_runs_generated_initializers() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();

    // `initialize_registered_metrics()` is process-global and only runs once.
    // Keep this as the only test in `base-metrics` that invokes it so the
    // assertions below are not order-dependent on another recorder scope.
    metrics::with_local_recorder(&recorder, || {
        base_metrics::initialize_registered_metrics();
    });

    let snapshot = snapshotter.snapshot().into_vec();
    assert_eq!(
        find_metric(&snapshot, MetricKind::Counter, "registry_test.ready_total"),
        Some(&DebugValue::Counter(0)),
    );
    assert_eq!(
        find_metric_labeled(
            &snapshot,
            MetricKind::Gauge,
            "registry_test.worker_state",
            &[("status", "idle")]
        ),
        Some(&DebugValue::Gauge(OrderedFloat(0.0))),
    );
    assert_eq!(
        find_metric_labeled(
            &snapshot,
            MetricKind::Gauge,
            "registry_test.worker_state",
            &[("status", "busy")]
        ),
        Some(&DebugValue::Gauge(OrderedFloat(0.0))),
    );
}
