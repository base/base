//! Metrics helpers for the sequencer actor.

use base_consensus_derive::AttributesBuilder;

use crate::{
    Conductor, Metrics, OriginSelector, SequencerActor, SequencerEngineClient,
    UnsafePayloadGossipClient,
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
        let active = if self.is_active { "true" } else { "false" };
        let recovery = if self.recovery_mode.get() { "true" } else { "false" };
        Metrics::sequencer_state(active, recovery).set(1.0);
    }
}
