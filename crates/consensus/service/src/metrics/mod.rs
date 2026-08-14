//! Metrics for the node service

use std::time::Duration;

use base_protocol::L2BlockInfo;

use crate::{ResetOrigin, ResetReason, ResetRequestOutcome};

base_metrics::define_metrics! {
    base_node
    #[describe("L1 reorg count")]
    l1_reorg_count: counter,
    #[describe("Derivation pipeline L1 origin")]
    derivation_l1_origin: counter,
    #[describe("Critical errors in the derivation pipeline")]
    derivation_critical_errors: counter,
    #[describe("Advisory follow-mode L1 origin check failures by reason")]
    #[label(name = "reason", default = ["unavailable", "fetch_failed", "not_canonical"])]
    follow_l1_origin_check_failures_total: counter,
    #[describe("Wall-clock duration of a single derivation pipeline step() call")]
    derivation_pipeline_step_duration_seconds: histogram,
    #[describe("Wall-clock duration the derivation actor waits for an inbound request")]
    derivation_actor_inbound_recv_wait_duration_seconds: histogram,
    #[describe("Tracks sequencer state flags")]
    #[label(active)]
    #[label(recovery)]
    sequencer_state: gauge,
    #[describe("Sequencer L1 origin RPC calls by method and outcome")]
    #[label(name = "method", default = ["block_by_hash", "block_by_number", "block_receipts"])]
    #[label(name = "outcome", default = ["success", "not_found", "timeout", "error"])]
    sequencer_l1_origin_rpc_calls_total: counter,
    #[describe("Wall-clock duration of sequencer L1 origin RPC calls in seconds")]
    #[label(name = "method", default = ["block_by_hash", "block_by_number", "block_receipts"])]
    #[label(name = "outcome", default = ["success", "not_found", "timeout", "error"])]
    sequencer_l1_origin_rpc_duration_seconds: histogram,
    #[describe("Canonical L1 successors that do not extend the accepted sequencer origin")]
    sequencer_l1_origin_orphans_total: counter,
    #[describe("Engine reset handling attempts by caller, cause, and outcome")]
    #[label(
        name = "origin",
        default = ["derivation", "sequencer", "shadow_cycle_coordinated"]
    )]
    #[label(
        name = "reason",
        default = [
            "derivation_pipeline",
            "derivation_l1_reorg",
            "l1_origin_unavailable",
            "l1_origin_orphaned",
            "l1_origin_inconsistent",
            "sequencer_startup",
            "admin",
            "shadow_cycle"
        ]
    )]
    #[label(
        name = "outcome",
        default = [
            "unchanged",
            "rewound",
            "deferred",
            "derivation_notification_failed",
            "failed"
        ]
    )]
    engine_reset_outcomes_total: counter,
    #[describe("Unsafe L2 head rewind depth caused by an engine reset")]
    #[label(
        name = "reason",
        default = [
            "derivation_pipeline",
            "derivation_l1_reorg",
            "l1_origin_unavailable",
            "l1_origin_orphaned",
            "l1_origin_inconsistent",
            "sequencer_startup",
            "admin",
            "shadow_cycle"
        ]
    )]
    engine_reset_unsafe_rewind_depth_blocks: histogram,
    #[describe("Wall-clock duration of one engine reset request handling attempt")]
    #[label(
        name = "reason",
        default = [
            "derivation_pipeline",
            "derivation_l1_reorg",
            "l1_origin_unavailable",
            "l1_origin_orphaned",
            "l1_origin_inconsistent",
            "sequencer_startup",
            "admin",
            "shadow_cycle"
        ]
    )]
    #[label(
        name = "outcome",
        default = [
            "unchanged",
            "rewound",
            "deferred",
            "derivation_notification_failed",
            "failed"
        ]
    )]
    engine_reset_duration_seconds: histogram,
    #[describe("Duration of the sequencer attributes builder")]
    sequencer_attributes_build_duration: histogram,
    #[describe("Duration of the sequencer block building start task")]
    sequencer_block_building_start_task_duration: histogram,
    #[describe("Duration of the sequencer block building seal task")]
    sequencer_block_building_seal_task_duration: histogram,
    #[describe("Total count of sequenced transactions")]
    sequencer_total_transactions_sequenced: counter,
    #[describe("Sequencer seal step retries by step")]
    #[label(name = "step", default = ["conductor", "gossip", "insert"])]
    sequencer_seal_step_retries_total: counter,
    #[describe("Sequencer seal step duration by step")]
    #[label(name = "step", default = ["conductor", "gossip", "insert"])]
    sequencer_seal_step_duration: histogram,
    #[describe("Wall-clock duration between successive successful seal completions (Ok(true) returns)")]
    sequencer_block_to_block_duration: histogram,
    #[describe("Wall-clock drift between the build-ticker target time and the actual fire time (>= 0; clamped to 0 when the ticker fires early)")]
    sequencer_ticker_drift_seconds: histogram,
    #[describe("Wall-clock duration of the full seal pipeline (conductor commit → gossip → engine insert), measured from PayloadSealer construction (after the EL seal response) until step() returns Ok(true). Excludes the EL build idle wait and the EL seal request.")]
    sequencer_seal_pipeline_duration: histogram,
    #[describe("Seal errors by fatality")]
    #[label(name = "fatal", default = ["true", "false"])]
    sequencer_seal_errors_total: counter,
    #[describe("Sequencer start rejections by reason")]
    #[label(name = "reason", default = ["not_leader", "leadership_check_failed"])]
    sequencer_start_rejected_total: counter,
    #[describe("Deferred stop_sequencer responses due to in-flight seal pipeline")]
    sequencer_stop_deferred_total: counter,
    #[describe("Blocks sequenced in recovery mode")]
    sequencer_recovery_mode_blocks_total: counter,
    #[describe("Empty blocks produced due to sequencer drift threshold")]
    sequencer_drift_empty_blocks_total: counter,
    #[describe("L1 origin lookups served from the selected-origin slot")]
    #[label(name = "kind", default = ["header", "receipts"])]
    sequencer_l1_origin_buffer_hits_total: counter,
    #[describe("L1 origin lookups that missed the selected-origin slot")]
    #[label(name = "kind", default = ["header", "receipts"])]
    sequencer_l1_origin_buffer_misses_total: counter,
    #[describe("Pre-built payloads discarded because the unsafe head advanced past their parent")]
    sequencer_stale_build_discarded_total: counter,
    #[describe("Configured verifier L1 confirmation depth")]
    l1_verifier_confs_depth: gauge,
    #[describe("L1 block number forwarded to derivation after verifier confirmation delay")]
    l1_verifier_derivation_head: counter,
    #[describe("Failed attempts to fetch a delayed L1 block for verifier confirmation")]
    l1_verifier_delayed_fetch_errors: counter,
}

impl Metrics {
    /// Records one engine reset handling attempt and any resulting unsafe-head rewind.
    pub fn record_engine_reset(
        origin: ResetOrigin,
        reason: ResetReason,
        outcome: ResetRequestOutcome,
        duration: Duration,
        unsafe_before: L2BlockInfo,
        unsafe_after: L2BlockInfo,
    ) {
        Self::engine_reset_outcomes_total(origin.as_str(), reason.as_str(), outcome.as_str())
            .increment(1);
        Self::engine_reset_duration_seconds(reason.as_str(), outcome.as_str()).record(duration);
        if unsafe_before.block_info.number != unsafe_after.block_info.number
            || unsafe_before.block_info.hash != unsafe_after.block_info.hash
        {
            Self::engine_reset_unsafe_rewind_depth_blocks(reason.as_str()).record(
                unsafe_before.block_info.number.saturating_sub(unsafe_after.block_info.number)
                    as f64,
            );
        }
    }
}

#[cfg(all(test, feature = "metrics"))]
mod tests {
    use alloy_primitives::B256;
    use base_protocol::{BlockInfo, L2BlockInfo};
    use metrics_util::{
        CompositeKey, MetricKind,
        debugging::{DebugValue, DebuggingRecorder},
    };

    use super::*;

    type SnapshotEntry =
        (CompositeKey, Option<metrics::Unit>, Option<metrics::SharedString>, DebugValue);

    fn metric<'a>(
        snapshot: &'a [SnapshotEntry],
        kind: MetricKind,
        name: &str,
        labels: &[(&str, &str)],
    ) -> Option<&'a DebugValue> {
        snapshot
            .iter()
            .find(|(key, _, _, _)| {
                key.kind() == kind
                    && key.key().name() == name
                    && labels.iter().all(|(expected_key, expected_value)| {
                        key.key().labels().any(|label| {
                            label.key() == *expected_key && label.value() == *expected_value
                        })
                    })
            })
            .map(|(_, _, _, value)| value)
    }

    fn head(number: u64, hash: u8) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                number,
                hash: B256::with_last_byte(hash),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn records_reset_outcomes_and_rewind_depths() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            Metrics::record_engine_reset(
                ResetOrigin::Sequencer,
                ResetReason::L1OriginOrphaned,
                ResetRequestOutcome::from_unsafe_heads(head(10, 1), head(10, 1)),
                Duration::from_millis(1),
                head(10, 1),
                head(10, 1),
            );
            Metrics::record_engine_reset(
                ResetOrigin::Sequencer,
                ResetReason::L1OriginOrphaned,
                ResetRequestOutcome::from_unsafe_heads(head(10, 1), head(7, 2)),
                Duration::from_millis(2),
                head(10, 1),
                head(7, 2),
            );
            Metrics::record_engine_reset(
                ResetOrigin::Sequencer,
                ResetReason::L1OriginOrphaned,
                ResetRequestOutcome::from_unsafe_heads(head(10, 1), head(10, 2)),
                Duration::from_millis(3),
                head(10, 1),
                head(10, 2),
            );
            Metrics::record_engine_reset(
                ResetOrigin::Sequencer,
                ResetReason::L1OriginOrphaned,
                ResetRequestOutcome::Deferred,
                Duration::from_millis(4),
                head(10, 1),
                head(10, 1),
            );
            Metrics::record_engine_reset(
                ResetOrigin::Sequencer,
                ResetReason::L1OriginOrphaned,
                ResetRequestOutcome::DerivationNotificationFailed,
                Duration::from_millis(5),
                head(10, 1),
                head(8, 2),
            );
            Metrics::record_engine_reset(
                ResetOrigin::Sequencer,
                ResetReason::L1OriginOrphaned,
                ResetRequestOutcome::Failed,
                Duration::from_millis(6),
                head(10, 1),
                head(10, 1),
            );
        });

        let snapshot = snapshotter.snapshot().into_vec();
        for (outcome, count) in [
            ("unchanged", 1),
            ("rewound", 2),
            ("deferred", 1),
            ("derivation_notification_failed", 1),
            ("failed", 1),
        ] {
            assert_eq!(
                metric(
                    &snapshot,
                    MetricKind::Counter,
                    "base_node.engine_reset_outcomes_total",
                    &[
                        ("origin", "sequencer"),
                        ("reason", "l1_origin_orphaned"),
                        ("outcome", outcome),
                    ],
                ),
                Some(&DebugValue::Counter(count)),
            );
        }

        match metric(
            &snapshot,
            MetricKind::Histogram,
            "base_node.engine_reset_unsafe_rewind_depth_blocks",
            &[("reason", "l1_origin_orphaned")],
        ) {
            Some(DebugValue::Histogram(values)) => {
                assert_eq!(
                    values.iter().map(|value| value.into_inner()).collect::<Vec<_>>(),
                    [3.0, 0.0, 2.0]
                );
            }
            value => panic!("expected rewind-depth observations, got {value:?}"),
        }

        for outcome in
            ["unchanged", "rewound", "deferred", "derivation_notification_failed", "failed"]
        {
            assert!(matches!(
                metric(
                    &snapshot,
                    MetricKind::Histogram,
                    "base_node.engine_reset_duration_seconds",
                    &[("reason", "l1_origin_orphaned"), ("outcome", outcome)],
                ),
                Some(DebugValue::Histogram(values)) if !values.is_empty()
            ));
        }
    }
}
