//! Snapshot helpers for base-metrics integration tests.

// Integration test binaries compile this module independently rather than exporting it.
#![allow(unreachable_pub)]

use metrics_util::{CompositeKey, MetricKind, debugging::DebugValue};

pub type SnapEntry =
    (CompositeKey, Option<metrics::Unit>, Option<metrics::SharedString>, DebugValue);

pub fn find_metric<'a>(
    snap: &'a [SnapEntry],
    kind: MetricKind,
    name: &str,
) -> Option<&'a DebugValue> {
    snap.iter()
        .find(|(ck, _, _, _)| ck.kind() == kind && ck.key().name() == name)
        .map(|(_, _, _, v)| v)
}

pub fn find_metric_labeled<'a>(
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
