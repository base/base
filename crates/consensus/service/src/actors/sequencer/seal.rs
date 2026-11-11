//! Payload sealer state machine.
//!
//! Tracks a sealed payload through the commit → gossip → insert pipeline,
//! retrying the current step on failure without rebuilding the payload.

use std::time::{Duration, Instant, SystemTime};

use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_protocol::L2BlockInfo;
use tracing::Instrument;

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
    /// Shadow sequencer payload ready for private engine insertion without commit or gossip.
    Private,
}

/// Result from one seal pipeline step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealStepOutcome {
    /// The current step completed and the pipeline has more work to do.
    Pending,
    /// The sealed payload has been inserted and acknowledged by the engine.
    Inserted(L2BlockInfo),
}

impl SealState {
    /// Returns a static label string for metrics (matches the metric label values).
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Sealed => "conductor",
            Self::Committed => "gossip",
            Self::Gossiped | Self::Private => "insert",
        }
    }
}

/// What prompted the sequencer to request a sealed payload from the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealTrigger {
    /// The execution layer reported the payload frozen before the seal deadline.
    ReadyHint,
    /// The seal deadline elapsed without a usable frozen-payload hint.
    Deadline,
}

impl SealTrigger {
    /// Returns the bounded metrics label for this trigger.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadyHint => "ready_hint",
            Self::Deadline => "deadline",
        }
    }
}

/// Places a seal within its block's slot, for reporting how early the block leaves the
/// sequencer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealSchedule {
    /// Wall-clock time the block's build was scheduled to start: its parent's timestamp.
    pub slot_start: SystemTime,
    /// What prompted the `getPayload` request.
    pub trigger: SealTrigger,
}

impl SealSchedule {
    /// Time elapsed since the slot started, or zero if the slot has not started yet.
    pub fn offset(&self) -> Duration {
        SystemTime::now().duration_since(self.slot_start).unwrap_or(Duration::ZERO)
    }
}

/// Drives a sealed payload through the commit → gossip → insert pipeline.
///
/// Each call to [`PayloadSealer::step`] performs exactly one async operation
/// based on the current [`SealState`]. On success the state advances; on
/// failure the state is unchanged so the same step is retried on the next call.
///
/// Once insertion succeeds, `step` returns [`SealStepOutcome::Inserted`] and the caller should
/// remove the sealer (the pipeline is complete).
#[derive(Debug)]
pub struct PayloadSealer {
    /// The sealed execution payload being driven through the pipeline.
    pub envelope: BaseExecutionPayloadEnvelope,
    /// Current pipeline stage.
    pub state: SealState,
    /// Span for the end-to-end seal pipeline lifecycle.
    pub seal_span: tracing::Span,
    /// Wall-clock instant the sealer was constructed (start of the seal pipeline).
    /// Read by the actor on `Ok(true)` to record
    /// [`crate::Metrics::sequencer_seal_pipeline_duration`].
    pub started_at: Instant,
    /// Slot timing of the seal that produced this payload, set by the actor. When present, the
    /// gossip step records [`crate::Metrics::sequencer_gossip_offset_seconds`].
    pub schedule: Option<SealSchedule>,
}

impl PayloadSealer {
    /// Creates a new sealer starting at the [`SealState::Sealed`] stage.
    pub fn new(envelope: BaseExecutionPayloadEnvelope) -> Self {
        let block_hash = envelope.execution_payload.block_hash();
        let block_num = envelope.execution_payload.block_number();
        let seal_span = tracing::info_span!(
            "seal_payload_pipeline",
            block_hash = %block_hash,
            block_number = block_num,
        );

        Self {
            envelope,
            state: SealState::Sealed,
            seal_span,
            started_at: Instant::now(),
            schedule: None,
        }
    }

    /// Creates a private sealer that skips conductor commit and gossip.
    ///
    /// `mode` distinguishes the reason payloads are sealed privately (e.g. `"shadow"` for a
    /// shadow-sequencing cycle, `"isolated"` for an isolated sequencer) so traces can tell the
    /// two apart.
    pub fn new_private(envelope: BaseExecutionPayloadEnvelope, mode: &'static str) -> Self {
        let block_hash = envelope.execution_payload.block_hash();
        let block_num = envelope.execution_payload.block_number();
        let seal_span = tracing::info_span!(
            "seal_payload_pipeline",
            block_hash = %block_hash,
            block_number = block_num,
            mode = mode,
        );

        Self {
            envelope,
            state: SealState::Private,
            seal_span,
            started_at: Instant::now(),
            schedule: None,
        }
    }

    /// Returns whether the engine acknowledged insertion of this sealer's exact payload.
    pub fn matches_inserted_head(&self, head: L2BlockInfo) -> bool {
        self.envelope.execution_payload.block_number() == head.block_info.number
            && self.envelope.execution_payload.block_hash() == head.block_info.hash
    }

    /// Performs one step of the seal pipeline.
    ///
    /// Returns [`SealStepOutcome::Inserted`] when the pipeline is complete.
    /// Returns [`SealStepOutcome::Pending`] when the step succeeded but more steps remain.
    /// Returns `Err` when the step failed — state is unchanged for retry.
    pub async fn step<C, G, E>(
        &mut self,
        conductor: &Option<C>,
        gossip_client: &G,
        engine_client: &E,
    ) -> Result<SealStepOutcome, SealStepError>
    where
        C: Conductor,
        G: UnsafePayloadGossipClient,
        E: SequencerEngineClient,
    {
        let step_label = self.state.label();
        let step_start = Instant::now();

        let result = match self.state {
            SealState::Sealed => {
                self.seal_span.in_scope(
                    || debug!(target: "sequencer", step = "commit", "seal pipeline step"),
                );
                if let Some(conductor) = conductor {
                    conductor
                        .commit_unsafe_payload(&self.envelope)
                        .instrument(self.seal_span.clone())
                        .await
                        .map_err(SealStepError::Conductor)?;
                }
                self.state = SealState::Committed;
                Ok(SealStepOutcome::Pending)
            }
            SealState::Committed => {
                self.seal_span.in_scope(
                    || debug!(target: "sequencer", step = "gossip", "seal pipeline step"),
                );
                gossip_client
                    .schedule_execution_payload_gossip(self.envelope.clone())
                    .instrument(self.seal_span.clone())
                    .await
                    .map_err(SealStepError::Gossip)?;
                if let Some(schedule) = self.schedule {
                    Metrics::sequencer_gossip_offset_seconds(schedule.trigger.as_str())
                        .record(schedule.offset());
                }
                self.state = SealState::Gossiped;
                Ok(SealStepOutcome::Pending)
            }
            SealState::Gossiped | SealState::Private => {
                self.seal_span.in_scope(|| {
                    debug!(
                        target: "sequencer",
                        step = "insert",
                        state = ?self.state,
                        "seal pipeline step"
                    );
                });
                let inserted_head = engine_client
                    .insert_unsafe_payload(self.envelope.clone())
                    .instrument(self.seal_span.clone())
                    .await
                    .map_err(SealStepError::Insert)?;
                Ok(SealStepOutcome::Inserted(inserted_head))
            }
        };

        match &result {
            Ok(_) => Metrics::sequencer_seal_step_duration(step_label).record(step_start.elapsed()),
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
