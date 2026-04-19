use alloy_primitives::B256;
use base_consensus_derive::AttributesBuilder;
use base_consensus_rpc::SequencerAdminAPIError;
use tokio::sync::oneshot;

use super::{SequencerActor, build::UnsealedPayloadHandle};
use crate::{Conductor, Metrics, OriginSelector, SequencerEngineClient, UnsafePayloadGossipClient};

/// The query types to the sequencer actor for the admin api.
#[derive(Debug)]
pub enum SequencerAdminQuery {
    /// A query to check if the sequencer is active.
    SequencerActive(oneshot::Sender<Result<bool, SequencerAdminAPIError>>),
    /// A query to start the sequencer.
    StartSequencer(B256, oneshot::Sender<Result<(), SequencerAdminAPIError>>),
    /// A query to stop the sequencer.
    StopSequencer(oneshot::Sender<Result<B256, SequencerAdminAPIError>>),
    /// A query to check if the conductor is enabled.
    ConductorEnabled(oneshot::Sender<Result<bool, SequencerAdminAPIError>>),
    /// A query to check if the sequencer is in recovery mode.
    RecoveryMode(oneshot::Sender<Result<bool, SequencerAdminAPIError>>),
    /// A query to set the recovery mode.
    SetRecoveryMode(bool, oneshot::Sender<Result<(), SequencerAdminAPIError>>),
    /// A query to override the leader.
    OverrideLeader(oneshot::Sender<Result<(), SequencerAdminAPIError>>),
    /// A query to reset the derivation pipeline.
    ResetDerivationPipeline(oneshot::Sender<Result<(), SequencerAdminAPIError>>),
}

/// Handler for the Sequencer Admin API.
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
    /// Handles the provided [`SequencerAdminQuery`], sending the response via the provided sender.
    /// This function is used to decouple admin API logic from the response mechanism (channels).
    pub(super) async fn handle_admin_query(
        &mut self,
        next_payload: &mut Option<UnsealedPayloadHandle>,
        query: SequencerAdminQuery,
    ) {
        match query {
            SequencerAdminQuery::SequencerActive(tx) => {
                if tx.send(self.is_sequencer_active().await).is_err() {
                    warn!(target: "sequencer", "Failed to send response for is_sequencer_active query");
                }
            }
            SequencerAdminQuery::StartSequencer(unsafe_head, tx) => {
                if tx.send(self.start_sequencer(unsafe_head).await).is_err() {
                    warn!(target: "sequencer", "Failed to send response for start_sequencer query");
                }
            }
            SequencerAdminQuery::StopSequencer(tx) => {
                self.stop_sequencer(next_payload, tx).await;
            }
            SequencerAdminQuery::ConductorEnabled(tx) => {
                if tx.send(self.is_conductor_enabled().await).is_err() {
                    warn!(target: "sequencer", "Failed to send response for is_conductor_enabled query");
                }
            }
            SequencerAdminQuery::RecoveryMode(tx) => {
                if tx.send(self.in_recovery_mode().await).is_err() {
                    warn!(target: "sequencer", "Failed to send response for in_recovery_mode query");
                }
            }
            SequencerAdminQuery::SetRecoveryMode(is_active, tx) => {
                if tx.send(self.set_recovery_mode(is_active).await).is_err() {
                    warn!(target: "sequencer", is_active = is_active, "Failed to send response for set_recovery_mode query");
                }
            }
            SequencerAdminQuery::OverrideLeader(tx) => {
                if tx.send(self.override_leader().await).is_err() {
                    warn!(target: "sequencer", "Failed to send response for override_leader query");
                }
            }
            SequencerAdminQuery::ResetDerivationPipeline(tx) => {
                if tx.send(self.reset_derivation_pipeline().await).is_err() {
                    warn!(target: "sequencer", "Failed to send response for reset_derivation_pipeline query");
                }
            }
        }
    }

    /// Returns whether the sequencer is active.
    pub(super) async fn is_sequencer_active(&self) -> Result<bool, SequencerAdminAPIError> {
        Ok(self.is_active)
    }

    /// Returns whether the conductor is enabled.
    pub(super) async fn is_conductor_enabled(&self) -> Result<bool, SequencerAdminAPIError> {
        Ok(self.conductor.is_some())
    }

    /// Returns whether the node is in recovery mode.
    pub(super) async fn in_recovery_mode(&self) -> Result<bool, SequencerAdminAPIError> {
        Ok(self.recovery_mode.get())
    }

    /// Starts the sequencer in an idempotent fashion.
    ///
    /// `unsafe_head` is a safety guard: it must match the engine's current unsafe head hash.
    /// This prevents split-brain situations where two nodes attempt to start sequencing from
    /// different chain tips. Activation is rejected when:
    ///
    /// - The engine has not yet received a forkchoice update (`unsafe_head == B256::ZERO`).
    /// - `unsafe_head` does not match the engine's current unsafe head hash.
    ///
    /// When a conductor is configured, this checks `conductor_leader` before activating,
    /// matching the reference node's `Start()` behavior. If the node is not the leader the call returns
    /// [`SequencerAdminAPIError::NotLeader`] and the sequencer remains inactive.
    pub(super) async fn start_sequencer(
        &mut self,
        unsafe_head: B256,
    ) -> Result<(), SequencerAdminAPIError> {
        if self.is_active {
            info!(target: "sequencer", unsafe_head = %unsafe_head, "received request to start sequencer, but it is already started");
            return Ok(());
        }

        if let Some(conductor) = &self.conductor {
            match conductor.leader().await {
                Ok(true) => {}
                Ok(false) => {
                    warn!(target: "sequencer", "Not the conductor leader, refusing to start sequencer");
                    Metrics::sequencer_start_rejected_total("not_leader").increment(1);
                    return Err(SequencerAdminAPIError::NotLeader);
                }
                Err(err) => {
                    error!(target: "sequencer", error = %err, "Failed to check conductor leadership");
                    Metrics::sequencer_start_rejected_total("leadership_check_failed").increment(1);
                    return Err(SequencerAdminAPIError::RequestError(err.to_string()));
                }
            }
        }

        let engine_head = self.engine_client.get_unsafe_head().await.map_err(|e| {
            error!(target: "sequencer", error = %e, "Failed to fetch engine unsafe head");
            SequencerAdminAPIError::RequestError(e.to_string())
        })?;

        if engine_head.block_info.hash == B256::ZERO {
            return Err(SequencerAdminAPIError::RequestError(
                "no prestate: engine unsafe head is uninitialized, cannot safely start sequencer"
                    .to_string(),
            ));
        }

        if unsafe_head != engine_head.block_info.hash {
            return Err(SequencerAdminAPIError::RequestError(format!(
                "block hash mismatch: engine unsafe head is {}, caller requested {}",
                engine_head.block_info.hash, unsafe_head,
            )));
        }

        info!(target: "sequencer", unsafe_head = %unsafe_head, "Starting sequencer");
        self.is_active = true;

        self.update_metrics();

        Ok(())
    }

    /// Stops the sequencer. If a seal pipeline is in-flight, the response is deferred
    /// until the pipeline completes so the returned hash reflects the fully inserted head.
    ///
    /// Any pre-built payload and stashed `next_build_parent` are discarded so that a subsequent
    /// restart always builds on a fresh, accurate head rather than a potentially stale one.
    pub(super) async fn stop_sequencer(
        &mut self,
        next_payload: &mut Option<UnsealedPayloadHandle>,
        tx: oneshot::Sender<Result<B256, SequencerAdminAPIError>>,
    ) {
        info!(target: "sequencer", "Stopping sequencer");
        self.is_active = false;
        // Discard any pre-built payload and stashed parent so a subsequent start_sequencer
        // always builds on a fresh, accurate head rather than a potentially stale one.
        next_payload.take();
        self.next_build_parent = None;
        self.update_metrics();

        if self.sealer.is_some() {
            info!(target: "sequencer", "Seal pipeline in-flight, deferring stop response");
            Metrics::sequencer_stop_deferred_total().increment(1);
            self.pending_stop = Some(tx);
        } else {
            let result = self.resolve_stop_head().await;
            if tx.send(result).is_err() {
                warn!(target: "sequencer", "Failed to send stop_sequencer response");
            }
        }
    }

    /// Returns the current unsafe head hash for the stop response.
    pub(super) async fn resolve_stop_head(&self) -> Result<B256, SequencerAdminAPIError> {
        self.engine_client.get_unsafe_head().await
            .map(|h| h.hash())
            .map_err(|e| {
                error!(target: "sequencer", err=?e, "Error fetching unsafe head after stopping sequencer");
                SequencerAdminAPIError::ErrorAfterSequencerWasStopped(
                    "current unsafe hash is unavailable.".to_string(),
                )
            })
    }

    /// Sets the recovery mode of the sequencer in an idempotent fashion.
    pub(super) async fn set_recovery_mode(
        &self,
        is_active: bool,
    ) -> Result<(), SequencerAdminAPIError> {
        self.recovery_mode.set(is_active);
        info!(target: "sequencer", is_active, "Updated recovery mode");

        self.update_metrics();

        Ok(())
    }

    /// Overrides the leader, if the conductor is enabled.
    /// If not, an error will be returned.
    pub(super) async fn override_leader(&mut self) -> Result<(), SequencerAdminAPIError> {
        let Some(conductor) = self.conductor.as_mut() else {
            return Err(SequencerAdminAPIError::LeaderOverrideError(
                "No conductor configured".to_string(),
            ));
        };

        if let Err(e) = conductor.override_leader().await {
            error!(target: "sequencer::rpc", error = %e, "Failed to override leader");
            return Err(SequencerAdminAPIError::LeaderOverrideError(e.to_string()));
        }
        info!(target: "sequencer", "Overrode leader via the conductor service");

        self.update_metrics();

        Ok(())
    }

    pub(super) async fn reset_derivation_pipeline(&self) -> Result<(), SequencerAdminAPIError> {
        info!(target: "sequencer", "Resetting derivation pipeline");
        self.engine_client.reset_engine_forkchoice().await.map_err(|e| {
            error!(target: "sequencer", err=?e, "Failed to reset engine forkchoice");
            SequencerAdminAPIError::RequestError(format!("Failed to reset engine: {e}"))
        })
    }
}
