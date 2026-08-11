//! Metrics helpers for the sequencer actor.

use std::time::{Duration, SystemTime};

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

    /// Records the signed drift between `block_number`'s seal target and the moment sealing
    /// actually begins. Positive values mean sealing started late; sustained positive drift
    /// that does not recover within 1–2 blocks indicates persistent overrun.
    pub(super) fn record_seal_target_drift(&self, block_number: u64, last_seal_duration: Duration) {
        let target = self.block_seal_target(block_number, last_seal_duration);
        let drift_seconds = match SystemTime::now().duration_since(target) {
            Ok(late) => late.as_secs_f64(),
            Err(early) => -early.duration().as_secs_f64(),
        };
        Metrics::sequencer_seal_target_drift_seconds().set(drift_seconds);
    }
}
