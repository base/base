use std::time::Duration;

use base_consensus_derive::AttributesBuilder;

use crate::{
    Conductor, OriginSelector, SequencerActor, SequencerEngineClient, UnsafePayloadGossipClient,
};

/// `SequencerActor` metrics-related method implementations.
impl<
    AttributesBuilder_,
    Conductor_,
    OriginSelector_,
    SequencerEngineClient_,
    UnsafePayloadGossipClient_,
>
    SequencerActor<
        AttributesBuilder_,
        Conductor_,
        OriginSelector_,
        SequencerEngineClient_,
        UnsafePayloadGossipClient_,
    >
where
    AttributesBuilder_: AttributesBuilder,
    Conductor_: Conductor,
    OriginSelector_: OriginSelector,
    SequencerEngineClient_: SequencerEngineClient,
    UnsafePayloadGossipClient_: UnsafePayloadGossipClient,
{
    /// Updates the metrics for the sequencer actor.
    pub(super) fn update_metrics(&self) {
        // no-op if disabled.
        #[cfg(feature = "metrics")]
        {
            let state_flags: [(&str, String); 2] = [
                ("active", self.is_active.to_string()),
                ("recovery", self.recovery_mode.get().to_string()),
            ];

            let gauge = metrics::gauge!(crate::Metrics::SEQUENCER_STATE, &state_flags);
            gauge.set(1);
        }
    }
}

#[inline]
pub(super) fn update_attributes_build_duration_metrics(_duration: Duration) {
    // Log the attributes build duration, if metrics are enabled.
    base_metrics::set!(gauge, crate::Metrics::SEQUENCER_ATTRIBUTES_BUILDER_DURATION, _duration);
}

#[inline]
pub(super) fn update_block_build_duration_metrics(_duration: Duration) {
    base_metrics::set!(
        gauge,
        crate::Metrics::SEQUENCER_BLOCK_BUILDING_START_TASK_DURATION,
        _duration
    );
}

#[inline]
pub(super) fn update_seal_duration_metrics(_duration: Duration) {
    // Log the block building seal task duration, if metrics are enabled.
    base_metrics::set!(
        gauge,
        crate::Metrics::SEQUENCER_BLOCK_BUILDING_SEAL_TASK_DURATION,
        _duration
    );
}

#[inline]
pub(super) fn update_total_transactions_sequenced(_transaction_count: u64) {
    #[cfg(feature = "metrics")]
    metrics::counter!(crate::Metrics::SEQUENCER_TOTAL_TRANSACTIONS_SEQUENCED)
        .increment(_transaction_count);
}

#[inline]
pub(super) fn inc_seal_step_retry(_step: &'static str) {
    base_metrics::inc!(counter, crate::Metrics::SEQUENCER_SEAL_STEP_RETRIES_TOTAL, "step" => _step);
}

#[inline]
pub(super) fn update_seal_step_duration(_step: &'static str, _duration: Duration) {
    base_metrics::set!(
        gauge,
        crate::Metrics::SEQUENCER_SEAL_STEP_DURATION,
        "step",
        _step,
        _duration
    );
}

#[inline]
pub(super) fn inc_seal_error(fatal: bool) {
    let _label = if fatal { "true" } else { "false" };
    base_metrics::inc!(counter, crate::Metrics::SEQUENCER_SEAL_ERROR_TOTAL, "fatal" => _label);
}

#[inline]
pub(super) fn inc_start_rejected(_reason: &'static str) {
    base_metrics::inc!(counter, crate::Metrics::SEQUENCER_START_REJECTED_TOTAL, "reason" => _reason);
}

#[inline]
pub(super) fn inc_stop_deferred() {
    base_metrics::inc!(counter, crate::Metrics::SEQUENCER_STOP_DEFERRED_TOTAL);
}

#[inline]
pub(super) fn inc_recovery_mode_block() {
    base_metrics::inc!(counter, crate::Metrics::SEQUENCER_RECOVERY_MODE_BLOCKS_TOTAL);
}

#[inline]
pub(super) fn inc_drift_empty_block() {
    base_metrics::inc!(counter, crate::Metrics::SEQUENCER_DRIFT_EMPTY_BLOCKS_TOTAL);
}

#[inline]
pub(super) fn inc_stale_build_discarded() {
    base_metrics::inc!(counter, crate::Metrics::SEQUENCER_STALE_BUILD_DISCARDED_TOTAL);
}
