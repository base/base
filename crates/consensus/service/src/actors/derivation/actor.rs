//! [`NodeActor`] implementation for the derivation sub-routine.

use std::sync::Arc;

use alloy_primitives::B256;
use async_trait::async_trait;
use base_consensus_derive::{
    ActivationSignal, Pipeline, PipelineError, PipelineErrorKind, ResetError, ResetSignal, Signal,
    SignalReceiver, StepResult,
};
use base_consensus_safedb::SafeHeadListener;
use base_protocol::{BlockInfo, OpAttributesWithParent};
use thiserror::Error;
use tokio::{select, sync::mpsc};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

#[cfg(feature = "metrics")]
use crate::Metrics;
use crate::{
    CancellableContext, DerivationActorRequest, DerivationEngineClient, DerivationState,
    DerivationStateMachine, DerivationStateTransitionError, DerivationStateUpdate, NodeActor,
    actors::derivation::L2Finalizer,
};

/// The [`NodeActor`] for the derivation sub-routine.
///
/// This actor is responsible for receiving messages from [`NodeActor`]s and stepping the
/// derivation pipeline forward to produce new payload attributes. The actor then sends the payload
/// to the [`NodeActor`] responsible for the execution sub-routine.
#[derive(Debug)]
pub struct DerivationActor<DerivationEngineClient_, PipelineSignalReceiver>
where
    DerivationEngineClient_: DerivationEngineClient,
    PipelineSignalReceiver: Pipeline + SignalReceiver,
{
    /// The cancellation token, shared between all tasks.
    cancellation_token: CancellationToken,
    /// The channel on which all inbound requests are received by the [`DerivationActor`].
    inbound_request_rx: mpsc::Receiver<DerivationActorRequest>,
    /// The Engine client used to interact with the engine.
    engine_client: DerivationEngineClient_,

    /// The derivation pipeline.
    pipeline: PipelineSignalReceiver,
    /// The state machine controlling when derivation can occur.
    derivation_state_machine: DerivationStateMachine,
    /// The [`L2Finalizer`] tracks derived L2 blocks awaiting finalization.
    pub(crate) finalizer: L2Finalizer,
    /// The safe head database listener for recording L1→L2 safe head mappings.
    safe_head_listener: Arc<dyn SafeHeadListener>,
    /// The L1 inclusion block for the most recently sent (unconfirmed) payload attributes.
    ///
    /// Set in [`attempt_derivation`] when attributes are dispatched to the engine; consumed in
    /// [`ProcessEngineSafeHeadUpdateRequest`] to key the `SafeDB` entry by inclusion block rather
    /// than epoch origin. `None` until the first derivation step, or after the value is consumed.
    ///
    /// [`attempt_derivation`]: Self::attempt_derivation
    /// [`ProcessEngineSafeHeadUpdateRequest`]: DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest
    pending_derived_from: Option<BlockInfo>,
}

impl<DerivationEngineClient_, PipelineSignalReceiver> CancellableContext
    for DerivationActor<DerivationEngineClient_, PipelineSignalReceiver>
where
    DerivationEngineClient_: DerivationEngineClient,
    PipelineSignalReceiver: Pipeline + SignalReceiver + Send + Sync,
{
    fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.cancellation_token.cancelled()
    }
}

impl<DerivationEngineClient_, PipelineSignalReceiver>
    DerivationActor<DerivationEngineClient_, PipelineSignalReceiver>
where
    DerivationEngineClient_: DerivationEngineClient,
    PipelineSignalReceiver: Pipeline + SignalReceiver,
{
    /// Creates a new instance of the [`DerivationActor`].
    pub fn new(
        engine_client: DerivationEngineClient_,
        cancellation_token: CancellationToken,
        inbound_request_rx: mpsc::Receiver<DerivationActorRequest>,
        pipeline: PipelineSignalReceiver,
        safe_head_listener: Arc<dyn SafeHeadListener>,
    ) -> Self {
        Self {
            cancellation_token,
            pipeline,
            inbound_request_rx,
            engine_client,
            derivation_state_machine: DerivationStateMachine::default(),
            finalizer: L2Finalizer::default(),
            safe_head_listener,
            pending_derived_from: None,
        }
    }

    /// Handles a [`Signal`] received over the derivation signal receiver channel.
    async fn signal(&mut self, signal: Signal) {
        if let Signal::Reset(ResetSignal { l1_origin: _l1_origin, .. }) = signal {
            base_metrics::set!(counter, Metrics::DERIVATION_L1_ORIGIN, _l1_origin.number);
            // Clear the finalization queue on reset.
            self.finalizer.clear();
            // Discard any in-flight derived_from so that a stale pre-reset L1 inclusion
            // block is never recorded for a post-reset safe head confirmation.
            self.pending_derived_from = None;

            // Reset the SafeDB on pipeline reset.
            //
            // On the very first reset (before any block is confirmed),
            // `last_confirmed_safe_head()` returns `L2BlockInfo::default()` (genesis / all
            // zeros), which causes the DB to wipe all entries and re-anchor at L1=0. This
            // is the correct full-truncation behaviour for a genesis reset.
            //
            // If this fails (disk full, corrupted DB), derivation continues but the DB may
            // be in an inconsistent state — subsequent RPC queries could return stale data.
            if let Err(e) = self
                .safe_head_listener
                .safe_head_reset(self.derivation_state_machine.last_confirmed_safe_head())
                .await
            {
                error!(target: "derivation", error = %e, "failed to reset safe head db — DB may be inconsistent");
            }
        }

        match self.pipeline.signal(signal).await {
            Ok(_) => info!(target: "derivation", ?signal, "[SIGNAL] Executed Successfully"),
            Err(e) => {
                error!(target: "derivation", ?e, ?signal, "Failed to signal derivation pipeline")
            }
        }
    }

    /// Attempts to step the derivation pipeline forward as much as possible in order to produce the
    /// next safe payload.
    async fn produce_next_attributes(&mut self) -> Result<OpAttributesWithParent, DerivationError> {
        // As we start the safe head at the disputed block's parent, we step the pipeline until the
        // first attributes are produced. All batches at and before the safe head will be
        // dropped, so the first payload will always be the disputed one.
        loop {
            match self.pipeline.step(self.derivation_state_machine.last_confirmed_safe_head()).await
            {
                StepResult::PreparedAttributes => { /* continue; attributes will be sent off. */ }
                StepResult::AdvancedOrigin => {
                    let origin =
                        self.pipeline.origin().ok_or(PipelineError::MissingOrigin.crit())?.number;

                    base_metrics::set!(counter, Metrics::DERIVATION_L1_ORIGIN, origin);
                    debug!(target: "derivation", l1_block = origin, "Advanced L1 origin");
                }
                StepResult::OriginAdvanceErr(e) | StepResult::StepFailed(e) => {
                    match e {
                        PipelineErrorKind::Temporary(e) => {
                            // NotEnoughData is transient, and doesn't imply we need to wait for
                            // more data. We can continue stepping until we receive an Eof.
                            if matches!(e, PipelineError::NotEnoughData) {
                                continue;
                            }

                            debug!(
                                target: "derivation",
                                "Exhausted data source for now; Yielding until the chain has extended."
                            );
                            return Err(DerivationError::Yield);
                        }
                        PipelineErrorKind::Reset(e) => {
                            warn!(target: "derivation", error = %e, "Derivation pipeline is being reset");

                            let system_config = self
                                .pipeline
                                .system_config_by_number(
                                    self.derivation_state_machine
                                        .last_confirmed_safe_head()
                                        .block_info
                                        .number,
                                )
                                .await?;

                            if matches!(e, ResetError::HoloceneActivation) {
                                let l1_origin = self
                                    .pipeline
                                    .origin()
                                    .ok_or(PipelineError::MissingOrigin.crit())?;

                                self.pipeline
                                    .signal(
                                        ActivationSignal {
                                            l2_safe_head: self
                                                .derivation_state_machine
                                                .last_confirmed_safe_head(),
                                            l1_origin,
                                            system_config: Some(system_config),
                                        }
                                        .signal(),
                                    )
                                    .await?;
                            } else {
                                if let ResetError::ReorgDetected(expected, new) = e {
                                    warn!(
                                        target: "derivation",
                                        "L1 reorg detected! Expected: {expected} | New: {new}"
                                    );

                                    base_metrics::inc!(counter, Metrics::L1_REORG_COUNT);
                                }
                                self.engine_client.reset_engine_forkchoice().await.map_err(|e| {
                                    error!(target: "derivation", ?e, "Failed to send reset request");
                                    DerivationError::Sender(Box::new(e))
                                })?;
                                self.derivation_state_machine
                                    .update(&DerivationStateUpdate::SignalNeeded)?;
                                return Err(DerivationError::Yield);
                            }
                        }
                        PipelineErrorKind::Critical(_) => {
                            error!(target: "derivation", error = %e, "Critical derivation error");
                            base_metrics::inc!(counter, Metrics::DERIVATION_CRITICAL_ERROR);
                            return Err(e.into());
                        }
                    }
                }
            }

            // If there are any new attributes, send them to the execution actor.
            if let Some(attrs) = self.pipeline.next() {
                return Ok(attrs);
            }
        }
    }

    async fn handle_derivation_actor_request(
        &mut self,
        request_type: DerivationActorRequest,
    ) -> Result<(), DerivationError> {
        match request_type {
            DerivationActorRequest::ProcessEngineSignalRequest(signal) => {
                self.signal(*signal).await;
                self.derivation_state_machine.update(&DerivationStateUpdate::SignalProcessed)?;
            }
            DerivationActorRequest::ProcessFinalizedL1Block(finalized_l1_block) => {
                // Attempt to finalize the block. If successful, notify engine.
                if let Some(l2_block_number) = self.finalizer.try_finalize_next(*finalized_l1_block)
                {
                    self.engine_client
                        .send_finalized_l2_block(l2_block_number)
                        .await
                        .map_err(|e| DerivationError::Sender(Box::new(e)))?;
                }
            }
            DerivationActorRequest::ProcessL1HeadUpdateRequest(l1_head) => {
                info!(target: "derivation", l1_head = ?*l1_head, "Processing l1 head update");

                self.derivation_state_machine.update(&DerivationStateUpdate::L1DataReceived)?;

                self.attempt_derivation().await?;
            }
            DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(safe_head) => {
                info!(target: "derivation", safe_head = ?*safe_head, "Received safe head from engine.");

                // Key the SafeDB entry by the L1 inclusion block (the L1 block whose data
                // contained the batch), not the L2 block's epoch origin. This gives finer
                // granularity: each batch's outcome is tracked at the L1 block where it landed.
                //
                // `pending_derived_from` is set in `attempt_derivation` just before the attrs
                // are sent to the engine. It is `None` only for EL-sync safe heads that were
                // not produced by local derivation; in that case fall back to epoch origin.
                let l1_block = self.pending_derived_from.take().unwrap_or(BlockInfo {
                    number: safe_head.l1_origin.number,
                    hash: safe_head.l1_origin.hash,
                    parent_hash: B256::ZERO,
                    timestamp: 0,
                });
                if let Err(e) =
                    self.safe_head_listener.safe_head_updated(*safe_head, l1_block).await
                {
                    error!(target: "derivation", error = %e, "failed to record safe head update");
                }

                self.derivation_state_machine
                    .update(&DerivationStateUpdate::NewAttributesConfirmed(safe_head))?;

                self.attempt_derivation().await?;
            }
            DerivationActorRequest::ProcessEngineSyncCompletionRequest(safe_head) => {
                info!(target: "derivation", "Engine finished syncing, starting derivation.");

                // Reset SafeDB when EL sync completes — the safe head may have advanced
                // without derivation, making prior SafeDB entries inaccurate.
                //
                // Note: this only deletes entries at or after reset_safe_head.l1_origin and
                // re-anchors there. Entries written by prior derivation runs at L1 blocks
                // *before* l1_origin are preserved. If those earlier entries were produced on a
                // now-diverged chain (e.g. a reorg that triggered EL sync), they are stale;
                // derivation will overwrite them incrementally as it re-derives from genesis.
                // Queries for L1 blocks before l1_origin may return stale data until
                // derivation catches up to those blocks.
                if let Err(e) = self.safe_head_listener.safe_head_reset(*safe_head).await {
                    error!(target: "derivation", error = %e, "failed to reset safe head db on EL sync completion");
                } else {
                    debug!(target: "derivation", l1_origin = safe_head.l1_origin.number, "reset safedb on EL sync; entries before this L1 origin are not backfilled");
                }

                self.derivation_state_machine
                    .update(&DerivationStateUpdate::ELSyncCompleted(safe_head))?;

                self.attempt_derivation().await?;
            }
        }

        Ok(())
    }

    /// Attempts to process the next payload attributes.
    async fn attempt_derivation(&mut self) -> Result<(), DerivationError> {
        if self.derivation_state_machine.current_state() != DerivationState::Deriving {
            info!(target: "derivation", derivation_state=?self.derivation_state_machine, "Skipping derivation.");
            return Ok(());
        }

        info!(target: "derivation", derivation_state=self.derivation_state_machine.confirmed_safe_head.block_info.number, "Attempting derivation.");
        debug!(target: "derivation", derivation_state=?self.derivation_state_machine, "Attempting derivation.");

        // Advance the pipeline as much as possible, new data may be available or there still may be
        // payloads in the attributes queue.
        let payload_attributes = match self.produce_next_attributes().await {
            Ok(attrs) => attrs,
            Err(DerivationError::Yield) => {
                info!(target: "derivation", "Yielding derivation until more data is available.");
                self.derivation_state_machine.update(&DerivationStateUpdate::MoreDataNeeded)?;
                return Ok(());
            }
            Err(e) => {
                return Err(e);
            }
        };
        trace!(target: "derivation", ?payload_attributes, "Produced payload attributes.");

        self.derivation_state_machine.update(&DerivationStateUpdate::NewAttributesDerived(
            Box::new(payload_attributes.clone()),
        ))?;

        // Enqueue the payload attributes for finalization tracking.
        self.finalizer.enqueue_for_finalization(&payload_attributes);

        // Remember the L1 inclusion block so that when the engine confirms this safe head we
        // can key the SafeDB entry by inclusion block rather than epoch origin.
        self.pending_derived_from = payload_attributes.derived_from;

        // Send payload attributes out for processing.
        self.engine_client
            .send_safe_l2_signal(payload_attributes.into())
            .await
            .map_err(|e| DerivationError::Sender(Box::new(e)))?;

        Ok(())
    }
}

#[async_trait]
impl<DerivationEngineClient_, PipelineSignalReceiver> NodeActor
    for DerivationActor<DerivationEngineClient_, PipelineSignalReceiver>
where
    DerivationEngineClient_: DerivationEngineClient + 'static,
    PipelineSignalReceiver: Pipeline + SignalReceiver + Send + Sync + 'static,
{
    type Error = DerivationError;
    type StartData = ();

    async fn start(mut self, _: Self::StartData) -> Result<(), Self::Error> {
        info!(target: "derivation", "Starting derivation");
        loop {
            select! {
                biased;

                _ = self.cancellation_token.cancelled() => {
                    info!(
                        target: "derivation",
                        "Received shutdown signal. Exiting derivation task."
                    );
                    return Ok(());
                }
                req = self.inbound_request_rx.recv() => {
                    let Some(request_type) = req else {
                        error!(target: "derivation", "DerivationActor inbound request receiver closed unexpectedly");
                        self.cancellation_token.cancel();
                        return Err(DerivationError::RequestReceiveFailed);
                    };

                    self.handle_derivation_actor_request(request_type).await?;
                }
            }
        }
    }
}

/// An error from the [`DerivationActor`].
#[derive(Error, Debug)]
pub enum DerivationError {
    /// An error originating from the derivation pipeline.
    #[error(transparent)]
    Pipeline(#[from] PipelineErrorKind),
    /// Waiting for more data to be available.
    #[error("Waiting for more data to be available")]
    Yield,
    /// An error originating from the broadcast sender.
    #[error("Failed to send event to broadcast sender: {0}")]
    Sender(Box<dyn std::error::Error + Send>),
    /// Failed to receive inbound request
    #[error("Failed to receive inbound request")]
    RequestReceiveFailed,
    /// An invalid state transition occurred.
    #[error(transparent)]
    StateTransitionError(#[from] DerivationStateTransitionError),
}
