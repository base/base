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
    #[cfg(feature = "metrics")]
    pub(super) fn update_metrics(&self) {
        let state_flags: [(&str, String); 2] = [
            ("active", self.is_active.to_string()),
            ("recovery", self.in_recovery_mode.to_string()),
        ];

        let gauge = metrics::gauge!(crate::Metrics::SEQUENCER_STATE, &state_flags);
        gauge.set(1);
    }

    /// Updates the metrics for the sequencer actor.
    #[cfg(not(feature = "metrics"))]
    pub(super) const fn update_metrics(&self) {}
}

#[cfg(feature = "metrics")]
#[inline]
pub(super) fn update_attributes_build_duration_metrics(duration: Duration) {
    base_macros::set!(gauge, crate::Metrics::SEQUENCER_ATTRIBUTES_BUILDER_DURATION, duration);
}

#[cfg(not(feature = "metrics"))]
#[inline]
pub(super) const fn update_attributes_build_duration_metrics(_: Duration) {}

#[cfg(feature = "metrics")]
#[inline]
pub(super) fn update_conductor_commitment_duration_metrics(duration: Duration) {
    base_macros::set!(gauge, crate::Metrics::SEQUENCER_CONDUCTOR_COMMITMENT_DURATION, duration);
}

#[cfg(not(feature = "metrics"))]
#[inline]
pub(super) const fn update_conductor_commitment_duration_metrics(_: Duration) {}

#[cfg(feature = "metrics")]
#[inline]
pub(super) fn update_block_build_duration_metrics(duration: Duration) {
    base_macros::set!(
        gauge,
        crate::Metrics::SEQUENCER_BLOCK_BUILDING_START_TASK_DURATION,
        duration
    );
}

#[cfg(not(feature = "metrics"))]
#[inline]
pub(super) const fn update_block_build_duration_metrics(_: Duration) {}

#[cfg(feature = "metrics")]
#[inline]
pub(super) fn update_seal_duration_metrics(duration: Duration) {
    base_macros::set!(gauge, crate::Metrics::SEQUENCER_BLOCK_BUILDING_SEAL_TASK_DURATION, duration);
}

#[cfg(not(feature = "metrics"))]
#[inline]
pub(super) const fn update_seal_duration_metrics(_: Duration) {}

#[cfg(feature = "metrics")]
#[inline]
pub(super) fn update_total_transactions_sequenced(transaction_count: u64) {
    metrics::counter!(crate::Metrics::SEQUENCER_TOTAL_TRANSACTIONS_SEQUENCED)
        .increment(transaction_count);
}

#[cfg(not(feature = "metrics"))]
#[inline]
pub(super) const fn update_total_transactions_sequenced(_: u64) {}
