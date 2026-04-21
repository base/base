//! Integration tests for the base-metrics macros and types.

use metrics_util::{
    CompositeKey, MetricKind,
    debugging::{DebugValue, DebuggingRecorder, Snapshotter},
};
use ordered_float::OrderedFloat;

type SnapEntry = (CompositeKey, Option<metrics::Unit>, Option<metrics::SharedString>, DebugValue);

fn with_recorder(f: impl FnOnce(Snapshotter)) {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, || f(snapshotter));
}

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

fn find_description(snap: &[SnapEntry], kind: MetricKind, name: &str) -> Option<String> {
    snap.iter()
        .find(|(ck, _, _, _)| ck.kind() == kind && ck.key().name() == name)
        .and_then(|(_, _, desc, _)| desc.as_ref().map(|d| d.to_string()))
}

fn assert_single_histogram(snap: &[SnapEntry], name: &str, min: f64) {
    match find_metric(snap, MetricKind::Histogram, name) {
        Some(DebugValue::Histogram(values)) => {
            assert_eq!(values.len(), 1, "expected exactly one histogram sample for {name}");
            assert!(
                values[0].into_inner() >= min,
                "expected >= {min} for {name}, got {}",
                values[0]
            );
        }
        other => panic!("expected histogram with one value for {name}, got {other:?}"),
    }
}

base_metrics::define_metrics! {
    test_app,
    struct = AppMetrics,
    #[describe("Total requests")]
    requests_total: counter,

    #[describe("Current connections")]
    active_connections: gauge,

    #[describe("Request duration in seconds")]
    request_duration: histogram,
}

#[test]
fn counter_increment() {
    with_recorder(|snap| {
        AppMetrics::requests_total().increment(5);

        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_metric(&snapshot, MetricKind::Counter, "test_app.requests_total"),
            Some(&DebugValue::Counter(5)),
        );
    });
}

#[test]
fn gauge_set() {
    with_recorder(|snap| {
        AppMetrics::active_connections().set(10.0);

        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_metric(&snapshot, MetricKind::Gauge, "test_app.active_connections"),
            Some(&DebugValue::Gauge(OrderedFloat(10.0))),
        );
    });
}

#[test]
fn histogram_record() {
    with_recorder(|snap| {
        AppMetrics::request_duration().record(0.42);
        AppMetrics::request_duration().record(1.23);

        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_metric(&snapshot, MetricKind::Histogram, "test_app.request_duration"),
            Some(&DebugValue::Histogram(vec![OrderedFloat(0.42), OrderedFloat(1.23)])),
        );
    });
}

#[test]
fn describe_registers_descriptions() {
    with_recorder(|snap| {
        AppMetrics::describe();
        AppMetrics::requests_total().increment(1);
        AppMetrics::active_connections().set(1.0);
        AppMetrics::request_duration().record(1.0);

        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_description(&snapshot, MetricKind::Counter, "test_app.requests_total").as_deref(),
            Some("Total requests"),
        );
        assert_eq!(
            find_description(&snapshot, MetricKind::Gauge, "test_app.active_connections")
                .as_deref(),
            Some("Current connections"),
        );
        assert_eq!(
            find_description(&snapshot, MetricKind::Histogram, "test_app.request_duration")
                .as_deref(),
            Some("Request duration in seconds"),
        );
    });
}

base_metrics::define_metrics! {
    my_service,
    struct = CustomMetrics,
    #[describe("Events processed")]
    events: counter,
}

#[test]
fn named_struct() {
    with_recorder(|snap| {
        CustomMetrics::events().increment(42);

        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_metric(&snapshot, MetricKind::Counter, "my_service.events"),
            Some(&DebugValue::Counter(42)),
        );
    });
}

base_metrics::define_metrics! {
    labeled_app,
    struct = LabeledMetrics,

    #[describe("Requests by method")]
    #[label(method)]
    requests: counter,

    #[describe("Latency by endpoint")]
    #[label(endpoint)]
    latency: histogram,

    #[describe("Active by status")]
    #[label(status)]
    active: gauge,
}

#[test]
fn single_label_counter() {
    with_recorder(|snap| {
        LabeledMetrics::requests("GET").increment(3);
        LabeledMetrics::requests("POST").increment(7);

        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Counter,
                "labeled_app.requests",
                &[("method", "GET")]
            ),
            Some(&DebugValue::Counter(3)),
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Counter,
                "labeled_app.requests",
                &[("method", "POST")]
            ),
            Some(&DebugValue::Counter(7)),
        );
    });
}

#[test]
fn single_label_histogram() {
    with_recorder(|snap| {
        LabeledMetrics::latency("/api/v1").record(0.05);

        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Histogram,
                "labeled_app.latency",
                &[("endpoint", "/api/v1")]
            ),
            Some(&DebugValue::Histogram(vec![OrderedFloat(0.05)])),
        );
    });
}

#[test]
fn single_label_gauge() {
    with_recorder(|snap| {
        LabeledMetrics::active("healthy").set(5.0);

        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Gauge,
                "labeled_app.active",
                &[("status", "healthy")]
            ),
            Some(&DebugValue::Gauge(OrderedFloat(5.0))),
        );
    });
}

base_metrics::define_metrics! {
    multi_label,
    struct = TwoLabelMetrics,

    #[describe("Errors by kind and reason")]
    #[label(kind)]
    #[label(reason)]
    errors: counter,
}

#[test]
fn two_label_counter() {
    with_recorder(|snap| {
        TwoLabelMetrics::errors("dial", "timeout").increment(2);
        TwoLabelMetrics::errors("dial", "refused").increment(1);

        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Counter,
                "multi_label.errors",
                &[("kind", "dial"), ("reason", "timeout")]
            ),
            Some(&DebugValue::Counter(2)),
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Counter,
                "multi_label.errors",
                &[("kind", "dial"), ("reason", "refused")]
            ),
            Some(&DebugValue::Counter(1)),
        );
    });
}

base_metrics::define_metrics! {
    timer_test,
    struct = TimerMetrics,
    #[describe("Duration")]
    duration: histogram,
}

#[test]
fn timed_records_on_drop() {
    with_recorder(|snap| {
        {
            let _timer = base_metrics::timed!(TimerMetrics::duration());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_single_histogram(&snap.snapshot().into_vec(), "timer_test.duration", 0.01);
    });
}

#[test]
fn timed_stop_records_early() {
    with_recorder(|snap| {
        let mut timer = base_metrics::timed!(TimerMetrics::duration());
        std::thread::sleep(std::time::Duration::from_millis(10));
        timer.stop();

        assert_single_histogram(&snap.snapshot().into_vec(), "timer_test.duration", 0.01);
    });
}

#[test]
fn timed_stop_is_idempotent() {
    with_recorder(|snap| {
        {
            let mut timer = base_metrics::timed!(TimerMetrics::duration());
            timer.stop();
            timer.stop();
        }

        let snapshot = snap.snapshot().into_vec();
        match find_metric(&snapshot, MetricKind::Histogram, "timer_test.duration") {
            Some(DebugValue::Histogram(values)) => {
                assert_eq!(values.len(), 1, "should record exactly once")
            }
            other => panic!("expected histogram with one value, got {other:?}"),
        }
    });
}

base_metrics::define_metrics! {
    time_block,
    struct = TimeBlockMetrics,
    #[describe("Duration")]
    duration: histogram,
}

#[test]
fn time_block_records_and_returns_value() {
    with_recorder(|snap| {
        let result = base_metrics::time!(TimeBlockMetrics::duration(), {
            std::thread::sleep(std::time::Duration::from_millis(10));
            42
        });

        assert_eq!(result, 42);
        assert_single_histogram(&snap.snapshot().into_vec(), "time_block.duration", 0.01);
    });
}

base_metrics::define_metrics! {
    inflight_test,
    struct = InflightMetrics,
    #[describe("In-flight operations")]
    in_flight: gauge,
}

#[test]
fn inflight_increments_and_decrements_gauge() {
    with_recorder(|snap| {
        {
            let _guard = base_metrics::inflight!(InflightMetrics::in_flight());

            let snapshot = snap.snapshot().into_vec();
            assert_eq!(
                find_metric(&snapshot, MetricKind::Gauge, "inflight_test.in_flight"),
                Some(&DebugValue::Gauge(OrderedFloat(1.0))),
            );
        }
        // After drop, the decrement should have fired. The snapshot swapped the
        // gauge to 0 on the previous read, so the net value is now -1.
        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_metric(&snapshot, MetricKind::Gauge, "inflight_test.in_flight"),
            Some(&DebugValue::Gauge(OrderedFloat(-1.0))),
        );
    });
}

#[test]
fn metric_names_use_dot_separator() {
    base_metrics::define_metrics! {
        scope_test
        #[describe("A counter")]
        my_counter: counter,
        #[describe("A gauge")]
        my_gauge: gauge,
        #[describe("A histogram")]
        my_histogram: histogram,
    }

    with_recorder(|snap| {
        Metrics::my_counter().increment(1);
        Metrics::my_gauge().set(1.0);
        Metrics::my_histogram().record(1.0);

        let snapshot = snap.snapshot().into_vec();
        let names: Vec<&str> = snapshot.iter().map(|(ck, _, _, _)| ck.key().name()).collect();
        assert!(names.contains(&"scope_test.my_counter"));
        assert!(names.contains(&"scope_test.my_gauge"));
        assert!(names.contains(&"scope_test.my_histogram"));
    });
}

base_metrics::define_metrics! {
    param_test,
    struct = ParamMetrics,

    #[describe("Counter with string label")]
    #[label(endpoint)]
    string_counter: counter,
}

#[test]
fn label_accepts_string() {
    with_recorder(|snap| {
        ParamMetrics::string_counter("/api/v1").increment(1);
        ParamMetrics::string_counter(String::from("/api/v2")).increment(2);

        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Counter,
                "param_test.string_counter",
                &[("endpoint", "/api/v1")]
            ),
            Some(&DebugValue::Counter(1)),
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Counter,
                "param_test.string_counter",
                &[("endpoint", "/api/v2")]
            ),
            Some(&DebugValue::Counter(2)),
        );
    });
}

base_metrics::define_metrics! {
    named_label_test,
    struct = NamedLabelMetrics,

    #[describe("Counter with explicit label name")]
    #[label(name = "endpoint")]
    requests: counter,
}

#[test]
fn explicit_label_name_without_defaults_works() {
    with_recorder(|snap| {
        NamedLabelMetrics::requests("/ready").increment(4);
        NamedLabelMetrics::zero();

        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Counter,
                "named_label_test.requests",
                &[("endpoint", "/ready")]
            ),
            Some(&DebugValue::Counter(4)),
        );
    });
}

base_metrics::define_metrics! {
    zero_test,
    struct = ZeroMetrics,

    #[describe("Unlabeled counter")]
    unlabeled_counter: counter,

    #[describe("Unlabeled gauge")]
    unlabeled_gauge: gauge,

    #[describe("Counter with defaults")]
    #[label(name = "status", default = ["ok", "error"])]
    labeled_counter: counter,

    #[describe("Gauge with defaults")]
    #[label(name = "state", default = ["open", "closed"])]
    labeled_gauge: gauge,

    #[describe("Two label counter with defaults")]
    #[label(name = "kind", default = ["network", "storage"])]
    #[label(name = "reason", default = ["timeout", "reset"])]
    multi_counter: counter,

    #[describe("Histogram with defaults")]
    #[label(name = "endpoint", default = ["/health"])]
    labeled_histogram: histogram,
}

#[test]
fn zero_initializes_unlabeled_and_labeled_metrics() {
    with_recorder(|snap| {
        ZeroMetrics::zero();

        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_metric(&snapshot, MetricKind::Counter, "zero_test.unlabeled_counter"),
            Some(&DebugValue::Counter(0)),
        );
        assert_eq!(
            find_metric(&snapshot, MetricKind::Gauge, "zero_test.unlabeled_gauge"),
            Some(&DebugValue::Gauge(OrderedFloat(0.0))),
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Counter,
                "zero_test.labeled_counter",
                &[("status", "ok")]
            ),
            Some(&DebugValue::Counter(0)),
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Counter,
                "zero_test.labeled_counter",
                &[("status", "error")]
            ),
            Some(&DebugValue::Counter(0)),
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Gauge,
                "zero_test.labeled_gauge",
                &[("state", "open")]
            ),
            Some(&DebugValue::Gauge(OrderedFloat(0.0))),
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Gauge,
                "zero_test.labeled_gauge",
                &[("state", "closed")]
            ),
            Some(&DebugValue::Gauge(OrderedFloat(0.0))),
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Counter,
                "zero_test.multi_counter",
                &[("kind", "network"), ("reason", "timeout")]
            ),
            Some(&DebugValue::Counter(0)),
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Counter,
                "zero_test.multi_counter",
                &[("kind", "network"), ("reason", "reset")]
            ),
            Some(&DebugValue::Counter(0)),
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Counter,
                "zero_test.multi_counter",
                &[("kind", "storage"), ("reason", "timeout")]
            ),
            Some(&DebugValue::Counter(0)),
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Counter,
                "zero_test.multi_counter",
                &[("kind", "storage"), ("reason", "reset")]
            ),
            Some(&DebugValue::Counter(0)),
        );
        assert_eq!(
            find_metric(&snapshot, MetricKind::Histogram, "zero_test.labeled_histogram"),
            None,
        );
    });
}

base_metrics::define_metrics! {
    no_zero_test,
    struct = NoZeroMetrics,

    #[describe("Unlabeled gauge that should not be zeroed")]
    #[no_zero]
    state_gauge: gauge,

    #[describe("Unlabeled counter that should not be zeroed")]
    #[no_zero]
    skipped_counter: counter,

    #[describe("Labeled gauge with defaults that should not be zeroed")]
    #[no_zero]
    #[label(name = "kind", default = ["one", "two"])]
    labeled_state_gauge: gauge,

    #[describe("Sibling gauge that should still be zeroed")]
    sibling_gauge: gauge,
}

#[test]
fn no_zero_skips_zero_initialization() {
    with_recorder(|snap| {
        NoZeroMetrics::zero();

        let snapshot = snap.snapshot().into_vec();

        // The opted-out metrics should not appear in the snapshot at all.
        assert_eq!(find_metric(&snapshot, MetricKind::Gauge, "no_zero_test.state_gauge"), None,);
        assert_eq!(
            find_metric(&snapshot, MetricKind::Counter, "no_zero_test.skipped_counter"),
            None,
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Gauge,
                "no_zero_test.labeled_state_gauge",
                &[("kind", "one")]
            ),
            None,
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Gauge,
                "no_zero_test.labeled_state_gauge",
                &[("kind", "two")]
            ),
            None,
        );

        // The sibling without `#[no_zero]` should still be zeroed.
        assert_eq!(
            find_metric(&snapshot, MetricKind::Gauge, "no_zero_test.sibling_gauge"),
            Some(&DebugValue::Gauge(OrderedFloat(0.0))),
        );
    });
}

#[test]
fn no_zero_metrics_can_still_be_set_explicitly() {
    with_recorder(|snap| {
        NoZeroMetrics::zero();
        NoZeroMetrics::state_gauge().set(42.0);
        NoZeroMetrics::skipped_counter().increment(5);
        NoZeroMetrics::labeled_state_gauge("one").set(7.0);

        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_metric(&snapshot, MetricKind::Gauge, "no_zero_test.state_gauge"),
            Some(&DebugValue::Gauge(OrderedFloat(42.0))),
        );
        assert_eq!(
            find_metric(&snapshot, MetricKind::Counter, "no_zero_test.skipped_counter"),
            Some(&DebugValue::Counter(5)),
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Gauge,
                "no_zero_test.labeled_state_gauge",
                &[("kind", "one")]
            ),
            Some(&DebugValue::Gauge(OrderedFloat(7.0))),
        );
    });
}

#[test]
fn no_zero_metrics_are_still_described_by_init() {
    with_recorder(|snap| {
        NoZeroMetrics::init();
        // Touch the metric so it appears in the snapshot alongside its description.
        NoZeroMetrics::state_gauge().set(1.0);

        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_description(&snapshot, MetricKind::Gauge, "no_zero_test.state_gauge").as_deref(),
            Some("Unlabeled gauge that should not be zeroed"),
        );
    });
}

#[test]
fn init_describes_and_zeroes_metrics() {
    with_recorder(|snap| {
        ZeroMetrics::init();
        ZeroMetrics::labeled_histogram("/health").record(0.25);

        let snapshot = snap.snapshot().into_vec();
        assert_eq!(
            find_description(&snapshot, MetricKind::Counter, "zero_test.unlabeled_counter")
                .as_deref(),
            Some("Unlabeled counter"),
        );
        assert_eq!(
            find_description(&snapshot, MetricKind::Gauge, "zero_test.labeled_gauge").as_deref(),
            Some("Gauge with defaults"),
        );
        assert_eq!(
            find_description(&snapshot, MetricKind::Histogram, "zero_test.labeled_histogram")
                .as_deref(),
            Some("Histogram with defaults"),
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Counter,
                "zero_test.labeled_counter",
                &[("status", "ok")]
            ),
            Some(&DebugValue::Counter(0)),
        );
        assert_eq!(
            find_metric_labeled(
                &snapshot,
                MetricKind::Histogram,
                "zero_test.labeled_histogram",
                &[("endpoint", "/health")]
            ),
            Some(&DebugValue::Histogram(vec![OrderedFloat(0.25)])),
        );
    });
}
