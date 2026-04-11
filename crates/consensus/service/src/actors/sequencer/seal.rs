//! Payload sealer state machine.
//!
//! Tracks a sealed payload through the commit → gossip → insert pipeline,
//! retrying the current step on failure without rebuilding the payload.

use std::time::Instant;

use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;

use crate::{
    Metrics, UnsafePayloadGossipClient,
    actors::{SequencerEngineClient, sequencer::conductor::Conductor},
};

/// Tracks where a sealed payload is in the commit → gossip → insert pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealState {
    /// Ready for conductor commit.
    Sealed,
    /// Conductor accepted. Ready for gossip.
    Committed,
    /// Gossiped to peers. Ready for engine insertion.
    Gossiped,
}

impl SealState {
    /// Returns a static label string for metrics (matches the metric label values).
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Sealed => "conductor",
            Self::Committed => "gossip",
            Self::Gossiped => "insert",
        }
    }
}

/// Drives a sealed payload through the commit → gossip → insert pipeline.
///
/// Each call to [`PayloadSealer::step`] performs exactly one async operation
/// based on the current [`SealState`]. On success the state advances; on
/// failure the state is unchanged so the same step is retried on the next call.
///
/// Once insertion succeeds, `step` returns `Ok(true)` and the caller should
/// remove the sealer (the pipeline is complete).
#[derive(Debug)]
pub struct PayloadSealer {
    /// The sealed execution payload being driven through the pipeline.
    pub envelope: BaseExecutionPayloadEnvelope,
    /// Current pipeline stage.
    pub state: SealState,
}

impl PayloadSealer {
    /// Creates a new sealer starting at the [`SealState::Sealed`] stage.
    pub const fn new(envelope: BaseExecutionPayloadEnvelope) -> Self {
        Self { envelope, state: SealState::Sealed }
    }

    /// Performs one step of the seal pipeline.
    ///
    /// Returns `Ok(true)` when the pipeline is complete (payload inserted).
    /// Returns `Ok(false)` when the step succeeded but more steps remain.
    /// Returns `Err` when the step failed — state is unchanged for retry.
    pub async fn step<C, G, E>(
        &mut self,
        conductor: &Option<C>,
        gossip_client: &G,
        engine_client: &E,
    ) -> Result<bool, SealStepError>
    where
        C: Conductor,
        G: UnsafePayloadGossipClient,
        E: SequencerEngineClient,
    {
        let step_label = self.state.label();
        let step_start = Instant::now();

        let result = match self.state {
            SealState::Sealed => {
                if let Some(conductor) = conductor {
                    conductor
                        .commit_unsafe_payload(&self.envelope)
                        .await
                        .map_err(SealStepError::Conductor)?;
                }
                self.state = SealState::Committed;
                Ok(false)
            }
            SealState::Committed => {
                gossip_client
                    .schedule_execution_payload_gossip(self.envelope.clone())
                    .await
                    .map_err(SealStepError::Gossip)?;
                self.state = SealState::Gossiped;
                Ok(false)
            }
            SealState::Gossiped => {
                engine_client
                    .insert_unsafe_payload(self.envelope.clone())
                    .await
                    .map_err(SealStepError::Insert)?;
                Ok(true)
            }
        };

        match &result {
            Ok(_) => Metrics::sequencer_seal_step_duration(step_label).set(step_start.elapsed()),
            Err(_) => Metrics::sequencer_seal_step_retries_total(step_label).increment(1),
        }

        result
    }
}

/// Errors from a single seal pipeline step.
#[derive(Debug, thiserror::Error)]
pub enum SealStepError {
    /// Conductor commit failed.
    #[error("conductor commit failed: {0}")]
    Conductor(crate::ConductorError),
    /// Gossip scheduling failed.
    #[error("gossip failed: {0}")]
    Gossip(crate::UnsafePayloadGossipClientError),
    /// Engine insertion failed.
    #[error("engine insert failed: {0}")]
    Insert(crate::actors::engine::EngineClientError),
}
