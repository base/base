//! [`NodeActor`] implementation for the derivation sub-routine.

use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use async_trait::async_trait;
use base_consensus_derive::{
    ActivationSignal, Pipeline, PipelineError, PipelineErrorKind, ResetError, ResetSignal, Signal,
    SignalReceiver, StepResult,
};
use base_consensus_safedb::SafeHeadListener;
use base_protocol::{AttributesWithParent, BlockInfo};
use thiserror::Error;
use tokio::{
    select,
    sync::{mpsc, watch},
    time::Instant,
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

use crate::{
    CancellableContext, DerivationActorRequest, DerivationEngineClient, DerivationState,
    DerivationStateMachine, DerivationStateTransitionError, DerivationStateUpdate, Metrics,
    NodeActor, ResetReason,
    actors::derivation::{L1CanonicalChain, L2Finalizer},
};

/// How long a failed L1 canonicity lookup waits before it is retried.
///
/// L1 finality can stall, so the retry cannot depend on a new finalized L1 signal: a transient L1
/// endpoint failure would otherwise withhold blocks whose inputs are already final for as long as
/// finality takes to move. The cooldown is short enough that a recovered endpoint resumes
/// finalization within a few L2 blocks, and long enough that a failing endpoint sees one request
/// per cooldown rather than one per derived block.
const CANONICALITY_RETRY_COOLDOWN: Duration = Duration::from_secs(5);

/// A canonicity lookup that failed and is waiting to be retried.
#[derive(Debug, Clone, Copy)]
struct CanonicalityDeferral {
    /// The finalized L1 height the lookup failed under. Advancing past it retries immediately.
    finalized_height: u64,
    /// When the lookup may be retried even if finality has not advanced.
    retry_at: Instant,
}

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
    /// Publishes the L1 origin the derivation pipeline has advanced to.
    derivation_origin_tx: watch::Sender<Option<BlockInfo>>,
    /// The state machine controlling when derivation can occur.
    derivation_state_machine: DerivationStateMachine,
    /// The [`L2Finalizer`] tracks derived L2 blocks awaiting finalization.
    pub finalizer: L2Finalizer,
    /// Confirms that a candidate's L1 inclusion block is still canonical before it is finalized.
    l1_canonical_chain: Arc<dyn L1CanonicalChain>,
    /// The canonicality lookup that failed, and when it may be retried.
    ///
    /// The candidate is kept, but the retry waits for finality to advance or for the cooldown to
    /// pass instead of repeating the lookup on every safe-head confirmation, which during catch-up
    /// is once per derived L2 block.
    canonicality_deferral: Option<CanonicalityDeferral>,
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
        derivation_origin_tx: watch::Sender<Option<BlockInfo>>,
        l1_canonical_chain: Arc<dyn L1CanonicalChain>,
    ) -> Self {
        Self {
            cancellation_token,
            pipeline,
            derivation_origin_tx,
            inbound_request_rx,
            engine_client,
            derivation_state_machine: DerivationStateMachine::default(),
            finalizer: L2Finalizer::default(),
            l1_canonical_chain,
            canonicality_deferral: None,
            safe_head_listener,
            pending_derived_from: None,
        }
    }

    fn publish_derivation_origin(&self) {
        self.derivation_origin_tx.send_replace(self.pipeline.origin());
    }

    /// Sends a finalized L2 block to the engine when the retained finalized L1 signal makes one
    /// eligible and its L1 inclusion block is still canonical.
    ///
    /// Verification is attempted at most once per finalized L1 update: a whole batch of L2 blocks
    /// can share one inclusion block, and a failed lookup is not retried on every safe-head
    /// confirmation. Within one attempt the number of L1 requests is bounded by the number of
    /// distinct inclusion identities checked, which a mismatch drop advances.
    ///
    /// Candidates whose inclusion block was replaced on L1 are dropped; a candidate whose lookup
    /// failed is kept until finality advances or the cooldown passes. Neither is ever finalized.
    async fn try_finalize_pending(&mut self) -> Result<(), DerivationError> {
        let Some(finalized_l1_block) = self.finalizer.finalized_l1_block() else {
            return Ok(());
        };

        // A failed lookup keeps its candidate, but retrying it on every safe-head confirmation
        // would turn an unavailable L1 endpoint into one request per derived block. Retry when
        // finality advances, or once the cooldown has passed: finality can stall for a long time,
        // so keying the retry on the finalized height alone would withhold blocks whose inputs are
        // already final until L1 finality happens to move again.
        if let Some(deferral) = self.canonicality_deferral {
            let finality_advanced = finalized_l1_block.number > deferral.finalized_height;
            if !finality_advanced && Instant::now() < deferral.retry_at {
                return Ok(());
            }
        }
        self.canonicality_deferral = None;

        while let Some(candidate) = self.finalizer.pending_candidate(finalized_l1_block.number) {
            // One verification covers every L2 block derived from the same inclusion block, so a
            // catching-up node does not make one L1 request per derived block.
            let canonical = if self.finalizer.verified_inclusion(candidate.inclusion) {
                Ok(true)
            } else {
                self.l1_canonical_chain.is_canonical(candidate.inclusion).await
            };

            match canonical {
                Ok(true) => {
                    self.finalizer.mark_inclusion_verified(candidate.inclusion);
                    if let Some(l2_block_number) =
                        self.finalizer.try_finalize_next(finalized_l1_block)
                    {
                        self.engine_client
                            .send_finalized_l2_block(l2_block_number)
                            .await
                            .map_err(|e| DerivationError::Sender(Box::new(e)))?;
                    }

                    return Ok(());
                }
                Ok(false) => {
                    // The batch that produced this block is no longer on the canonical L1 chain,
                    // so the block's inputs are not final. Drop it and keep checking lower
                    // candidates: the next one may still be canonical.
                    warn!(
                        target: "derivation",
                        inclusion_block = candidate.inclusion.number,
                        inclusion_hash = %candidate.inclusion.hash,
                        l2_block = candidate.l2_block,
                        "dropping derived candidate whose L1 inclusion block is not canonical",
                    );
                    self.finalizer.drop_candidate(candidate.inclusion.number);
                }
                Err(error) => {
                    // Never finalize on an unverified inclusion block. The candidate and the
                    // finalized L1 signal are both retained, and the check is retried once
                    // finality advances, not on every safe-head confirmation.
                    warn!(
                        target: "derivation",
                        inclusion_block = candidate.inclusion.number,
                        l2_block = candidate.l2_block,
                        %error,
                        "L1 inclusion canonicity lookup failed, deferring finalization",
                    );
                    self.canonicality_deferral = Some(CanonicalityDeferral {
                        finalized_height: finalized_l1_block.number,
                        retry_at: Instant::now() + CANONICALITY_RETRY_COOLDOWN,
                    });
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    /// Handles a [`Signal`] received over the derivation signal receiver channel.
    async fn signal(&mut self, signal: Signal) {
        if let Signal::Reset(ResetSignal { l2_safe_head: reset_safe_head }) = signal {
            Metrics::derivation_l1_origin().absolute(reset_safe_head.l1_origin.number);
            // Keep the candidates that the reset still allows to be finalized. Clearing the
            // whole queue used to lose every block at or below the confirmed safe head, which is
            // never re-derived, so a reset rate above the L1 finality cadence starved
            // finalization indefinitely. Nothing is finalized here: retention only keeps
            // candidates at or below the last confirmed safe head, and those were already offered
            // to the engine by the finalized L1 signal they were confirmed with.
            self.finalizer.retain_valid_after_reset(&reset_safe_head);
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
            Ok(_) => {
                self.publish_derivation_origin();
                info!(target: "derivation", ?signal, "[SIGNAL] Executed Successfully");
            }
            Err(e) => {
                error!(target: "derivation", ?e, ?signal, "Failed to signal derivation pipeline")
            }
        }
    }

    /// Attempts to step the derivation pipeline forward as much as possible in order to produce the
    /// next safe payload.
    async fn produce_next_attributes(&mut self) -> Result<AttributesWithParent, DerivationError> {
        // As we start the safe head at the disputed block's parent, we step the pipeline until the
        // first attributes are produced. All batches at and before the safe head will be
        // dropped, so the first payload will always be the disputed one.
        loop {
            let step_result =
                base_metrics::time!(Metrics::derivation_pipeline_step_duration_seconds(), {
                    self.pipeline
                        .step(self.derivation_state_machine.last_confirmed_safe_head())
                        .await
                });
            match step_result {
                StepResult::PreparedAttributes => { /* continue; attributes will be sent off. */ }
                StepResult::AdvancedOrigin => {
                    let origin =
                        self.pipeline.origin().ok_or(PipelineError::MissingOrigin.crit())?;

                    Metrics::derivation_l1_origin().absolute(origin.number);
                    self.derivation_origin_tx.send_replace(Some(origin));
                    debug!(target: "derivation", l1_block = origin.number, "Advanced L1 origin");
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

                            if matches!(e, ResetError::HoloceneActivation) {
                                self.pipeline
                                    .signal(
                                        ActivationSignal {
                                            l2_safe_head: self
                                                .derivation_state_machine
                                                .last_confirmed_safe_head(),
                                        }
                                        .signal(),
                                    )
                                    .await?;
                            } else {
                                let reason = if matches!(&e, ResetError::ReorgDetected(..)) {
                                    ResetReason::DerivationL1Reorg
                                } else {
                                    ResetReason::DerivationPipeline
                                };
                                if let ResetError::ReorgDetected(expected, new) = e {
                                    warn!(
                                        target: "derivation",
                                        %expected,
                                        %new,
                                        "L1 reorg detected"
                                    );

                                    Metrics::l1_reorg_count().increment(1);
                                }
                                self.engine_client
                                    .reset_engine_forkchoice(reason)
                                    .await
                                    .map_err(|e| {
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
                            Metrics::derivation_critical_errors().increment(1);
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
                // Retain the signal even when no derived L2 blocks are currently queued. The
                // finalizer will retry it after derivation rebuilds the safe head.
                self.finalizer.record_finalized_l1_block(*finalized_l1_block);
                self.try_finalize_pending().await?;
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
                let derived_from = self.pending_derived_from.take();
                let l1_block = derived_from.unwrap_or(BlockInfo {
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

                // Remember which L1 block this safe head's inputs came from. A later reset keeps
                // the derived candidates up to that inclusion block rather than only up to the
                // safe head's epoch origin, which is what makes retention independent of how
                // often the pipeline resets.
                self.finalizer.confirm_safe_head(safe_head.as_ref(), derived_from);

                self.derivation_state_machine
                    .update(&DerivationStateUpdate::NewAttributesConfirmed(safe_head))?;

                // A reset keeps the candidates it could not invalidate, and the finalized L1
                // signal is retained. Retry after the rebuilt safe head is confirmed, matching
                // op-node's safe-derived finalization trigger.
                self.try_finalize_pending().await?;

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
            #[cfg(test)]
            DerivationActorRequest::CurrentStateRequest(result_tx) => {
                if result_tx.send(self.derivation_state_machine.current_state()).is_err() {
                    warn!(target: "derivation", "failed to return derivation state to test observer");
                }
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
            .send_derived_attributes(payload_attributes)
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
            let mut recv_timer = base_metrics::timed!(
                Metrics::derivation_actor_inbound_recv_wait_duration_seconds()
            );
            select! {
                biased;

                _ = self.cancellation_token.cancelled() => {
                    recv_timer.disarm();
                    info!(
                        target: "derivation",
                        "Received shutdown signal. Exiting derivation task."
                    );
                    return Ok(());
                }
                req = self.inbound_request_rx.recv() => {
                    recv_timer.stop();
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

#[cfg(test)]
mod tests {
    //! Unit tests for the reset handling of the [`DerivationActor`].
    //!
    //! The pipeline double is hand-rolled rather than mocked with `mockall`: [`Pipeline`] is
    //! defined in `base-consensus-derive`, so an automock would have to live in that crate's test
    //! surface, and the double only needs to return a fixed `StepResult` so the actor never
    //! derives anything on its own.

    use std::sync::Arc;

    use alloy_eips::BlockNumHash;
    use alloy_primitives::B256;
    use base_common_genesis::RollupConfig;
    use base_common_rpc_types_engine::BasePayloadAttributes;
    use base_consensus_derive::{
        Pipeline, PipelineError, PipelineResult, ResetSignal, Signal, SignalReceiver, StepResult,
    };
    use base_consensus_safedb::SafeHeadListener;
    use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};
    use mockall::predicate::eq;
    use tokio::sync::{mpsc, watch};

    use super::{super::engine_client::MockDerivationEngineClient, DerivationActor};
    use crate::{
        DerivationActorRequest, L1CanonicalChain, MockL1CanonicalChain,
        test_utils::{AlwaysCanonical, FakeSafeDB},
    };

    /// The number of safe-head confirmations a catching-up node makes for the L2 blocks of a
    /// single L1 inclusion block, used to show that canonicality lookups do not follow them.
    const CATCH_UP_CONFIRMATIONS: usize = 100;

    /// A [`Pipeline`] double that reports end-of-data on every step and records reset signals.
    #[derive(Debug)]
    struct IdlePipeline {
        rollup_config: Arc<RollupConfig>,
        origin: Option<BlockInfo>,
    }

    impl IdlePipeline {
        fn new() -> Self {
            Self { rollup_config: Arc::new(RollupConfig::default()), origin: None }
        }
    }

    impl Iterator for IdlePipeline {
        type Item = AttributesWithParent;

        fn next(&mut self) -> Option<Self::Item> {
            None
        }
    }

    impl base_consensus_derive::OriginProvider for IdlePipeline {
        fn origin(&self) -> Option<BlockInfo> {
            self.origin
        }
    }

    #[async_trait::async_trait]
    impl Pipeline for IdlePipeline {
        fn peek(&self) -> Option<&AttributesWithParent> {
            None
        }

        async fn step(&mut self, _cursor: L2BlockInfo) -> StepResult {
            StepResult::StepFailed(PipelineError::Eof.temp())
        }

        fn rollup_config(&self) -> &RollupConfig {
            &self.rollup_config
        }

        async fn system_config_by_l2_hash(
            &mut self,
            _hash: alloy_primitives::B256,
        ) -> PipelineResult<base_common_genesis::SystemConfig> {
            Err(PipelineError::MissingOrigin.crit())
        }
    }

    #[async_trait::async_trait]
    impl SignalReceiver for IdlePipeline {
        async fn signal(&mut self, signal: Signal) -> PipelineResult<()> {
            if let Signal::Reset(ResetSignal { l2_safe_head }) = signal {
                self.origin = Some(BlockInfo {
                    number: l2_safe_head.l1_origin.number,
                    hash: l2_safe_head.l1_origin.hash,
                    ..Default::default()
                });
            }
            Ok(())
        }
    }

    /// Builds a derived [`AttributesWithParent`] for the L2 block after `parent_number`, with
    /// `inclusion_number` as the L1 inclusion block (`derived_from`).
    fn attrs(parent_number: u64, inclusion_number: u64) -> AttributesWithParent {
        let parent = L2BlockInfo {
            block_info: BlockInfo { number: parent_number, ..Default::default() },
            l1_origin: BlockNumHash::default(),
            seq_num: 0,
        };
        let derived_from = BlockInfo { number: inclusion_number, ..Default::default() };
        AttributesWithParent::new(
            BasePayloadAttributes::default(),
            parent,
            Some(derived_from),
            false,
        )
    }

    fn l1_at(number: u64) -> BlockInfo {
        BlockInfo { number, ..Default::default() }
    }

    /// Builds an L1 block at `number` with a distinct hash, so two blocks at the same height can
    /// be told apart the way an L1 reorg does.
    fn inclusion_at(number: u64, hash_byte: u8) -> BlockInfo {
        BlockInfo { number, hash: B256::with_last_byte(hash_byte), ..Default::default() }
    }

    /// Builds attributes whose derived L2 block number is `l2_parent_number + 1` and whose L1
    /// inclusion block is `inclusion`.
    fn attrs_with_inclusion(l2_parent_number: u64, inclusion: BlockInfo) -> AttributesWithParent {
        let parent = L2BlockInfo {
            block_info: BlockInfo { number: l2_parent_number, ..Default::default() },
            l1_origin: BlockNumHash::default(),
            seq_num: 0,
        };
        AttributesWithParent::new(BasePayloadAttributes::default(), parent, Some(inclusion), false)
    }

    fn safe_head_at(number: u64, origin_number: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo { number, ..Default::default() },
            l1_origin: BlockNumHash { number: origin_number, ..Default::default() },
            seq_num: 0,
        }
    }

    /// Builds an actor with no inbound traffic and a mock engine client that records the
    /// finalized L2 block numbers it is handed.
    fn test_actor(
        engine_client: MockDerivationEngineClient,
    ) -> (
        DerivationActor<MockDerivationEngineClient, IdlePipeline>,
        mpsc::Sender<DerivationActorRequest>,
    ) {
        test_actor_with_l1(engine_client, Arc::new(AlwaysCanonical))
    }

    /// Builds an actor whose canonical-L1 answers come from `l1_canonical_chain`.
    fn test_actor_with_l1(
        engine_client: MockDerivationEngineClient,
        l1_canonical_chain: Arc<dyn L1CanonicalChain>,
    ) -> (
        DerivationActor<MockDerivationEngineClient, IdlePipeline>,
        mpsc::Sender<DerivationActorRequest>,
    ) {
        let (request_tx, request_rx) = mpsc::channel(16);
        let (origin_tx, _origin_rx) = watch::channel(None);
        let actor = DerivationActor::new(
            engine_client,
            tokio_util::sync::CancellationToken::new(),
            request_rx,
            IdlePipeline::new(),
            Arc::new(FakeSafeDB::default()) as Arc<dyn SafeHeadListener>,
            origin_tx,
            l1_canonical_chain,
        );
        (actor, request_tx)
    }

    #[tokio::test]
    async fn derivation_resets_do_not_starve_finalization() {
        let mut engine_client = MockDerivationEngineClient::new();
        engine_client.expect_send_finalized_l2_block().times(1).with(eq(502)).returning(|_| Ok(()));
        let (mut actor, _request_tx) = test_actor(engine_client);

        // L1 finality has already reached block 100, and the derivation pipeline has derived L2
        // blocks whose batches were included in L1 blocks that are not final yet.
        assert!(
            actor
                .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                    Box::new(l1_at(100))
                ))
                .await
                .is_ok()
        );
        for (l2_parent, inclusion) in [(500, 105), (501, 107), (502, 110)] {
            actor.finalizer.enqueue_for_finalization(&attrs(l2_parent, inclusion));
        }

        // The pipeline resets repeatedly while the safe head keeps advancing. Each reset rebases
        // L1 traversal on an origin that trails the L1 block the newest batch landed in, and the
        // engine confirms the safe head each time before the next reset.
        for step in 0..3u64 {
            let mut safe_head = safe_head_at(503 + step, 104 + 2 * step);
            safe_head.block_info.hash = B256::with_last_byte(1 + step as u8);
            actor.pending_derived_from = Some(l1_at(110));
            actor
                .handle_derivation_actor_request(
                    DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(Box::new(safe_head)),
                )
                .await
                .expect("safe head update failed");
            actor.signal(ResetSignal { l2_safe_head: safe_head }.signal()).await;
        }

        // Finalized L1 now covers the inclusion block of the second derived block.
        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(108)),
            ))
            .await
            .expect("finalized L1 update failed");
    }

    #[tokio::test]
    async fn reset_above_the_confirmed_safe_head_drops_unconfirmed_candidates() {
        let mut engine_client = MockDerivationEngineClient::new();
        engine_client.expect_send_finalized_l2_block().times(1).with(eq(500)).returning(|_| Ok(()));
        let (mut actor, _request_tx) = test_actor(engine_client);

        actor.finalizer.confirm_safe_head(&safe_head_at(500, 110), Some(l1_at(112)));
        actor.finalizer.enqueue_for_finalization(&attrs(499, 110)); // l2=500, confirmed
        actor.finalizer.enqueue_for_finalization(&attrs(500, 112)); // l2=501, in flight

        // The reset walks back from the unsafe head and moves the safe head well above the last
        // confirmed block, so the block the engine holds at 501 need not be the one derivation
        // sent and must not be finalized.
        actor.signal(ResetSignal { l2_safe_head: safe_head_at(900, 300) }.signal()).await;

        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(400)),
            ))
            .await
            .expect("finalized L1 update failed");
    }

    #[tokio::test]
    async fn catch_up_canonicality_lookups_are_bounded_per_finalized_l1_update() {
        // A node catching up derives many L2 blocks that a batch included in one L1 block
        // produced, while a single finalized L1 signal stays latched. The inclusion block is
        // final, so one verification covers every L2 block behind it: the number of L1 lookups
        // must not follow the number of confirmations.
        let mut engine_client = MockDerivationEngineClient::new();
        engine_client
            .expect_send_finalized_l2_block()
            .times(CATCH_UP_CONFIRMATIONS)
            .returning(|_| Ok(()));

        let mut l1 = MockL1CanonicalChain::new();
        l1.expect_is_canonical().times(1).returning(|_| Ok(true));
        let (mut actor, _request_tx) = test_actor_with_l1(engine_client, Arc::new(l1));

        let inclusion_a = inclusion_at(120, 0xa);
        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(120)),
            ))
            .await
            .expect("finalized L1 update failed");

        for offset in 0..CATCH_UP_CONFIRMATIONS {
            let l2_block = 500 + offset as u64;
            actor
                .finalizer
                .enqueue_for_finalization(&attrs_with_inclusion(l2_block - 1, inclusion_a));
            actor.pending_derived_from = Some(inclusion_a);
            actor
                .handle_derivation_actor_request(
                    DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(Box::new(
                        safe_head_at(l2_block, 118),
                    )),
                )
                .await
                .expect("safe head update failed");
        }
    }

    /// Drives `count` safe-head confirmations of L2 blocks whose batches all landed in
    /// `inclusion`, the way a catching-up node confirms a long run behind one inclusion block.
    async fn confirm_run(
        actor: &mut DerivationActor<MockDerivationEngineClient, IdlePipeline>,
        first_l2_block: u64,
        count: u64,
        inclusion: BlockInfo,
    ) {
        for l2_block in first_l2_block..(first_l2_block + count) {
            actor
                .finalizer
                .enqueue_for_finalization(&attrs_with_inclusion(l2_block - 1, inclusion));
            actor.pending_derived_from = Some(inclusion);
            actor
                .handle_derivation_actor_request(
                    DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(Box::new(
                        safe_head_at(l2_block, 118),
                    )),
                )
                .await
                .expect("safe head update failed");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn deferred_lookup_retries_after_the_cooldown_without_new_finality() {
        let mut engine_client = MockDerivationEngineClient::new();
        // Everything held back while the lookup failed is promoted in one jump once it succeeds:
        // the confirmation that triggers the retry carries L2 601, and the queue drains to it.
        engine_client.expect_send_finalized_l2_block().times(1).with(eq(601)).returning(|_| Ok(()));

        // The lookup fails once and the provider recovers while L1 finality stands still. The
        // retry has to come from time, not from a new finalized L1 signal.
        let mut l1 = MockL1CanonicalChain::new();
        let mut sequence = mockall::Sequence::new();
        l1.expect_is_canonical()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|_| Err("l1 rpc unavailable".to_string()));
        l1.expect_is_canonical().times(1).in_sequence(&mut sequence).returning(|_| Ok(true));
        let (mut actor, _request_tx) = test_actor_with_l1(engine_client, Arc::new(l1));

        let inclusion_a = inclusion_at(120, 0xa);
        actor.finalizer.enqueue_for_finalization(&attrs_with_inclusion(499, inclusion_a));
        actor.finalizer.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_a));

        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(200)),
            ))
            .await
            .expect("finalized L1 update failed");

        // Confirmations inside the cooldown do not retry the failed lookup.
        confirm_run(&mut actor, 501, 100, inclusion_a).await;

        // Once the cooldown has passed, the next confirmation retries it and finalizes.
        tokio::time::advance(super::CANONICALITY_RETRY_COOLDOWN).await;
        confirm_run(&mut actor, 601, 1, inclusion_a).await;
    }

    #[tokio::test(start_paused = true)]
    async fn failing_lookup_is_retried_at_most_once_per_cooldown() {
        let mut engine_client = MockDerivationEngineClient::new();
        engine_client.expect_send_finalized_l2_block().times(0);

        // A persistently failing provider sees one request per cooldown: one when the failure is
        // recorded and one after the cooldown, however many confirmations arrive in between.
        let mut l1 = MockL1CanonicalChain::new();
        l1.expect_is_canonical().times(2).returning(|_| Err("l1 rpc unavailable".to_string()));
        let (mut actor, _request_tx) = test_actor_with_l1(engine_client, Arc::new(l1));

        let inclusion_a = inclusion_at(120, 0xa);
        actor.finalizer.enqueue_for_finalization(&attrs_with_inclusion(499, inclusion_a));
        actor.finalizer.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_a));

        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(200)),
            ))
            .await
            .expect("finalized L1 update failed");

        confirm_run(&mut actor, 501, 100, inclusion_a).await;

        tokio::time::advance(super::CANONICALITY_RETRY_COOLDOWN).await;
        confirm_run(&mut actor, 601, 1, inclusion_a).await;
        confirm_run(&mut actor, 602, 49, inclusion_a).await;
    }

    #[tokio::test]
    async fn failed_canonicality_lookup_is_retried_once_per_finalized_l1_update() {
        let mut engine_client = MockDerivationEngineClient::new();
        // The candidates held back while the lookup failed are promoted in one jump once it
        // succeeds.
        engine_client.expect_send_finalized_l2_block().times(1).with(eq(510)).returning(|_| Ok(()));

        // One lookup fails on the first finalized L1 update and one retries it after finality
        // advances; the confirmations in between must not retry it.
        let mut l1 = MockL1CanonicalChain::new();
        let mut sequence = mockall::Sequence::new();
        l1.expect_is_canonical()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|_| Err("l1 rpc unavailable".to_string()));
        l1.expect_is_canonical().times(1).in_sequence(&mut sequence).returning(|_| Ok(true));
        let (mut actor, _request_tx) = test_actor_with_l1(engine_client, Arc::new(l1));

        let inclusion_a = inclusion_at(120, 0xa);
        actor.finalizer.enqueue_for_finalization(&attrs_with_inclusion(499, inclusion_a));
        actor.finalizer.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_a));

        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(120)),
            ))
            .await
            .expect("finalized L1 update failed");

        for l2_block in 501..511 {
            actor
                .finalizer
                .enqueue_for_finalization(&attrs_with_inclusion(l2_block - 1, inclusion_a));
            actor.pending_derived_from = Some(inclusion_a);
            actor
                .handle_derivation_actor_request(
                    DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(Box::new(
                        safe_head_at(l2_block, 118),
                    )),
                )
                .await
                .expect("safe head update failed");
        }

        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(121)),
            ))
            .await
            .expect("finalized L1 update failed");
    }

    #[tokio::test]
    async fn verified_inclusion_does_not_vouch_for_a_different_block_at_the_same_height() {
        let mut engine_client = MockDerivationEngineClient::new();
        engine_client.expect_send_finalized_l2_block().times(1).with(eq(500)).returning(|_| Ok(()));

        // The canonical block at height 120 is 120/A. A candidate that claims 120/B is a
        // different inclusion identity, so verifying height 120 for 120/A must not vouch for it.
        let mut l1 = MockL1CanonicalChain::new();
        let mut sequence = mockall::Sequence::new();
        l1.expect_is_canonical()
            .withf(|block| block.hash == B256::with_last_byte(0xa))
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|_| Ok(true));
        l1.expect_is_canonical()
            .withf(|block| block.hash == B256::with_last_byte(0xb))
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|_| Ok(false));
        let (mut actor, _request_tx) = test_actor_with_l1(engine_client, Arc::new(l1));

        let inclusion_a = inclusion_at(120, 0xa);
        actor.finalizer.enqueue_for_finalization(&attrs_with_inclusion(499, inclusion_a));
        actor.finalizer.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_a));
        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(120)),
            ))
            .await
            .expect("finalized L1 update failed");

        // A stale candidate at the same height but on the replaced block shows up afterwards.
        let inclusion_b = inclusion_at(120, 0xb);
        actor.finalizer.enqueue_for_finalization(&attrs_with_inclusion(500, inclusion_b));
        actor.finalizer.confirm_safe_head(&safe_head_at(501, 118), Some(inclusion_b));
        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(120)),
            ))
            .await
            .expect("finalized L1 update failed");
    }
    #[tokio::test]
    async fn reorged_inclusion_is_not_finalized() {
        let mut engine_client = MockDerivationEngineClient::new();
        // The candidate's batch is no longer on the canonical L1 chain, so nothing may be
        // finalized.
        engine_client.expect_send_finalized_l2_block().times(0);

        let mut l1 = MockL1CanonicalChain::new();
        l1.expect_is_canonical()
            .withf(|block| block.number == 120 && block.hash == B256::with_last_byte(0xa))
            .times(1)
            .returning(|_| Ok(false));
        let (mut actor, _request_tx) = test_actor_with_l1(engine_client, Arc::new(l1));

        // L2 500 was derived from a batch included in L1 120/A, and 500 is the confirmed safe head
        // whose epoch origin is 118.
        let inclusion_a = inclusion_at(120, 0xa);
        actor.finalizer.enqueue_for_finalization(&attrs_with_inclusion(499, inclusion_a));
        actor.finalizer.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_a));

        // A reset keeps the same L2 safe head, so the candidate survives it.
        actor.signal(ResetSignal { l2_safe_head: safe_head_at(500, 118) }.signal()).await;

        // L1 finalizes height 120, where the canonical block is now 120/B.
        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(120)),
            ))
            .await
            .expect("finalized L1 update failed");
    }

    #[tokio::test]
    async fn canonical_inclusion_is_finalized() {
        let mut engine_client = MockDerivationEngineClient::new();
        engine_client.expect_send_finalized_l2_block().times(1).with(eq(500)).returning(|_| Ok(()));

        let mut l1 = MockL1CanonicalChain::new();
        l1.expect_is_canonical().times(1).returning(|_| Ok(true));
        let (mut actor, _request_tx) = test_actor_with_l1(engine_client, Arc::new(l1));

        let inclusion_a = inclusion_at(120, 0xa);
        actor.finalizer.enqueue_for_finalization(&attrs_with_inclusion(499, inclusion_a));
        actor.finalizer.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_a));
        actor.signal(ResetSignal { l2_safe_head: safe_head_at(500, 118) }.signal()).await;

        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(120)),
            ))
            .await
            .expect("finalized L1 update failed");
    }

    #[tokio::test]
    async fn failed_inclusion_lookup_defers_finalization() {
        let mut engine_client = MockDerivationEngineClient::new();
        engine_client.expect_send_finalized_l2_block().times(1).with(eq(500)).returning(|_| Ok(()));

        let mut l1 = MockL1CanonicalChain::new();
        let mut sequence = mockall::Sequence::new();
        l1.expect_is_canonical()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|_| Err("l1 rpc unavailable".to_string()));
        l1.expect_is_canonical().times(1).in_sequence(&mut sequence).returning(|_| Ok(true));
        let (mut actor, _request_tx) = test_actor_with_l1(engine_client, Arc::new(l1));

        let inclusion_a = inclusion_at(120, 0xa);
        actor.finalizer.enqueue_for_finalization(&attrs_with_inclusion(499, inclusion_a));
        actor.finalizer.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_a));
        actor.signal(ResetSignal { l2_safe_head: safe_head_at(500, 118) }.signal()).await;

        // The first update cannot verify the inclusion block: nothing is finalized and the
        // candidate is kept.
        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(120)),
            ))
            .await
            .expect("finalized L1 update failed");

        // The next update retries the check and finalizes.
        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(121)),
            ))
            .await
            .expect("finalized L1 update failed");
    }

    #[tokio::test]
    async fn inclusion_is_checked_only_when_a_candidate_is_eligible() {
        let mut engine_client = MockDerivationEngineClient::new();
        engine_client.expect_send_finalized_l2_block().times(1).with(eq(500)).returning(|_| Ok(()));

        // One lookup for the single update that makes the candidate eligible, none for the update
        // that is still below its inclusion block.
        let mut l1 = MockL1CanonicalChain::new();
        l1.expect_is_canonical().times(1).returning(|_| Ok(true));
        let (mut actor, _request_tx) = test_actor_with_l1(engine_client, Arc::new(l1));

        let inclusion_a = inclusion_at(120, 0xa);
        actor.finalizer.enqueue_for_finalization(&attrs_with_inclusion(499, inclusion_a));
        actor.finalizer.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_a));

        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(100)),
            ))
            .await
            .expect("finalized L1 update failed");

        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(120)),
            ))
            .await
            .expect("finalized L1 update failed");
    }

    #[tokio::test]
    async fn reset_keeps_candidates_derived_from_the_confirmed_safe_head() {
        let mut engine_client = MockDerivationEngineClient::new();
        engine_client.expect_send_finalized_l2_block().times(1).with(eq(502)).returning(|_| Ok(()));
        let (mut actor, _request_tx) = test_actor(engine_client);

        // The pipeline derived L2 block 502 from a batch included in L1 block 110, well above the
        // block's epoch origin (106), and the engine confirmed 502 as the safe head.
        actor.finalizer.enqueue_for_finalization(&attrs(501, 110));
        actor.pending_derived_from = Some(l1_at(110));
        actor
            .handle_derivation_actor_request(
                DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(Box::new(safe_head_at(
                    502, 106,
                ))),
            )
            .await
            .expect("safe head update failed");

        // A reset keeps the candidate: the confirmation recorded the L1 range the safe head was
        // built from, which includes the L1 block its own batch landed in.
        actor.signal(ResetSignal { l2_safe_head: safe_head_at(502, 106) }.signal()).await;

        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(110)),
            ))
            .await
            .expect("finalized L1 update failed");
    }

    #[tokio::test]
    async fn reset_rolling_back_the_safe_head_never_finalizes_above_it() {
        let mut engine_client = MockDerivationEngineClient::new();
        engine_client.expect_send_finalized_l2_block().times(1).with(eq(480)).returning(|_| Ok(()));
        let (mut actor, _request_tx) = test_actor(engine_client);

        actor.finalizer.confirm_safe_head(&safe_head_at(600, 100), Some(l1_at(108)));
        actor.finalizer.enqueue_for_finalization(&attrs(479, 105));
        actor.finalizer.enqueue_for_finalization(&attrs(599, 108));

        // The reset rolls the safe head back below the second candidate. Both inclusion blocks
        // sit at or below the reset origin, so only the rollback drops the second candidate.
        actor.signal(ResetSignal { l2_safe_head: safe_head_at(500, 110) }.signal()).await;

        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(120)),
            ))
            .await
            .expect("finalized L1 update failed");
    }

    #[tokio::test]
    async fn reset_drops_candidates_included_above_the_reset_origin() {
        let mut engine_client = MockDerivationEngineClient::new();
        engine_client.expect_send_finalized_l2_block().times(1).with(eq(480)).returning(|_| Ok(()));
        let (mut actor, _request_tx) = test_actor(engine_client);

        // Both candidates are below the reset safe head, but only the first was included at or
        // below the L1 range the engine's safe chain was built from.
        actor.finalizer.confirm_safe_head(&safe_head_at(600, 100), Some(l1_at(101)));
        actor.finalizer.enqueue_for_finalization(&attrs(479, 100));
        actor.finalizer.enqueue_for_finalization(&attrs(489, 101));

        actor.signal(ResetSignal { l2_safe_head: safe_head_at(500, 100) }.signal()).await;

        actor
            .handle_derivation_actor_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(l1_at(120)),
            ))
            .await
            .expect("finalized L1 update failed");
    }
}
