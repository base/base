//! [`NodeActor`] implementation for the derivation sub-routine.

use std::sync::Arc;

use alloy_primitives::B256;
use async_trait::async_trait;
use base_consensus_derive::{
    ActivationSignal, Pipeline, PipelineError, PipelineErrorKind, ResetError, ResetSignal, Signal,
    SignalReceiver, StepResult,
};
use base_consensus_safedb::SafeHeadListener;
use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};
use thiserror::Error;
use tokio::{
    select,
    sync::{mpsc, watch},
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

use crate::{
    AfterMailbox, AwaitingSafeHead, CancellableContext, DerivationActorRequest,
    DerivationEngineClient, DerivationState, Deriving, Idle, MailboxIdle, Metrics, NodeActor,
    ResetReason, actors::derivation::L2Finalizer,
};

/// Outcome of waiting in an idle derivation state.
#[derive(Debug)]
pub enum WaitOutcome {
    /// The cancellation token fired.
    Shutdown,
    /// Apply the mailbox (or oneshot) result.
    Next(AfterMailbox),
}

/// Result of stepping the pipeline while in [`Deriving`].
#[derive(Debug)]
pub enum ProduceAttributes {
    /// Payload attributes ready to send to the engine.
    Ready(Box<AttributesWithParent>),
    /// The pipeline is exhausted until more L1 data arrives.
    NeedMoreData,
    /// An engine reset was requested; wait for the resulting signal.
    NeedSignal,
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
    /// The [`L2Finalizer`] tracks derived L2 blocks awaiting finalization.
    pub finalizer: L2Finalizer,
    /// The safe head database listener for recording L1→L2 safe head mappings.
    safe_head_listener: Arc<dyn SafeHeadListener>,
    /// The L1 inclusion block for the most recently sent (unconfirmed) payload attributes.
    ///
    /// Set in [`Deriving::run`] when attributes are dispatched to the engine; consumed when a
    /// safe head is recorded so the `SafeDB` entry is keyed by inclusion block rather than epoch
    /// origin. `None` until the first derivation step, or after the value is consumed.
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
    ) -> Self {
        Self {
            cancellation_token,
            pipeline,
            derivation_origin_tx,
            inbound_request_rx,
            engine_client,
            finalizer: L2Finalizer::default(),
            safe_head_listener,
            pending_derived_from: None,
        }
    }

    fn publish_derivation_origin(&self) {
        self.derivation_origin_tx.send_replace(self.pipeline.origin());
    }

    /// Sends a finalized L2 block to the engine when the retained finalized L1 signal makes one
    /// eligible.
    async fn try_finalize_pending(&mut self) -> Result<(), DerivationError> {
        if let Some(l2_block_number) = self.finalizer.try_finalize_pending() {
            self.engine_client
                .send_finalized_l2_block(l2_block_number)
                .await
                .map_err(|e| DerivationError::Sender(Box::new(e)))?;
        }

        Ok(())
    }

    /// Records an engine safe-head update in the `SafeDB` and retries pending finalization.
    ///
    /// When `consume_pending` is set, the in-flight attributes' L1 inclusion block is used as
    /// the `SafeDB` key. Mailbox updates while waiting on derived-attribute confirmation must
    /// pass `false` so a leftover engine notification cannot steal the next batch's inclusion
    /// block.
    async fn record_engine_safe_head(
        &mut self,
        safe_head: L2BlockInfo,
        consume_pending: bool,
    ) -> Result<(), DerivationError> {
        let l1_block = if consume_pending { self.pending_derived_from.take() } else { None }
            .unwrap_or(BlockInfo {
                number: safe_head.l1_origin.number,
                hash: safe_head.l1_origin.hash,
                parent_hash: B256::ZERO,
                timestamp: 0,
            });
        if let Err(e) = self.safe_head_listener.safe_head_updated(safe_head, l1_block).await {
            error!(target: "derivation", error = %e, "failed to record safe head update");
        }

        self.try_finalize_pending().await
    }

    /// Handles a [`Signal`] received over the derivation signal receiver channel.
    async fn signal(&mut self, signal: Signal) {
        if let Signal::Reset(ResetSignal { l2_safe_head }) = signal {
            Metrics::derivation_l1_origin().absolute(l2_safe_head.l1_origin.number);
            self.finalizer.clear();
            self.pending_derived_from = None;

            if let Err(e) = self.safe_head_listener.safe_head_reset(l2_safe_head).await {
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
    async fn produce_next_attributes(
        &mut self,
        confirmed_safe_head: L2BlockInfo,
    ) -> Result<ProduceAttributes, DerivationError> {
        loop {
            let step_result =
                base_metrics::time!(Metrics::derivation_pipeline_step_duration_seconds(), {
                    self.pipeline.step(confirmed_safe_head).await
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
                StepResult::OriginAdvanceErr(e) | StepResult::StepFailed(e) => match e {
                    PipelineErrorKind::Temporary(e) => {
                        if matches!(e, PipelineError::NotEnoughData) {
                            continue;
                        }

                        debug!(
                            target: "derivation",
                            "Exhausted data source for now; Yielding until the chain has extended."
                        );
                        return Ok(ProduceAttributes::NeedMoreData);
                    }
                    PipelineErrorKind::Reset(e) => {
                        warn!(target: "derivation", error = %e, "Derivation pipeline is being reset");

                        if matches!(e, ResetError::HoloceneActivation) {
                            self.pipeline
                                .signal(
                                    ActivationSignal { l2_safe_head: confirmed_safe_head }.signal(),
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
                            self.engine_client.reset_engine_forkchoice(reason).await.map_err(
                                    |e| {
                                        error!(target: "derivation", ?e, "Failed to send reset request");
                                        DerivationError::Sender(Box::new(e))
                                    },
                                )?;
                            return Ok(ProduceAttributes::NeedSignal);
                        }
                    }
                    PipelineErrorKind::Critical(_) => {
                        error!(target: "derivation", error = %e, "Critical derivation error");
                        Metrics::derivation_critical_errors().increment(1);
                        return Err(e.into());
                    }
                },
            }

            if let Some(attrs) = self.pipeline.next() {
                return Ok(ProduceAttributes::Ready(Box::new(attrs)));
            }
        }
    }

    async fn handle_mailbox<IdleState: MailboxIdle>(
        &mut self,
        state: IdleState,
        request_type: DerivationActorRequest,
    ) -> Result<AfterMailbox, DerivationError> {
        match request_type {
            DerivationActorRequest::ProcessEngineSignalRequest(signal) => {
                self.signal(*signal).await;
                Ok(state.on_signal_processed(*signal))
            }
            DerivationActorRequest::ProcessFinalizedL1Block(finalized_l1_block) => {
                if let Some(l2_block_number) =
                    self.finalizer.process_finalized_l1_block(*finalized_l1_block)
                {
                    self.engine_client
                        .send_finalized_l2_block(l2_block_number)
                        .await
                        .map_err(|e| DerivationError::Sender(Box::new(e)))?;
                }
                Ok(AfterMailbox::Idle(state.into_idle()))
            }
            DerivationActorRequest::ProcessL1HeadUpdateRequest(l1_head) => {
                info!(target: "derivation", l1_head = ?*l1_head, "Processing l1 head update");
                Ok(state.on_l1_data())
            }
            DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(safe_head) => {
                info!(target: "derivation", safe_head = ?*safe_head, "Received safe head from engine.");
                let awaiting_confirmation =
                    state.projection() == DerivationState::AwaitingSafeHeadConfirmation;
                let before = state.confirmed_safe_head();
                let after = state.on_engine_safe_head(*safe_head);
                if after.confirmed_safe_head() != before {
                    self.record_engine_safe_head(*safe_head, !awaiting_confirmation).await?;
                }
                Ok(after)
            }
            DerivationActorRequest::ProcessEngineSyncCompletionRequest(safe_head) => {
                if state.projection() == DerivationState::AwaitingELSyncCompletion {
                    info!(target: "derivation", "Engine finished syncing, starting derivation.");
                    if let Err(e) = self.safe_head_listener.safe_head_reset(*safe_head).await {
                        error!(target: "derivation", error = %e, "failed to reset safe head db on EL sync completion");
                    } else {
                        debug!(target: "derivation", l1_origin = safe_head.l1_origin.number, "reset safedb on EL sync; entries before this L1 origin are not backfilled");
                    }
                }
                Ok(state.on_el_sync_completed(*safe_head))
            }
            #[cfg(test)]
            DerivationActorRequest::CurrentStateRequest(result_tx) => {
                if result_tx.send(state.projection()).is_err() {
                    warn!(target: "derivation", "failed to return derivation state to test observer");
                }
                Ok(AfterMailbox::Idle(state.into_idle()))
            }
            #[cfg(test)]
            DerivationActorRequest::CurrentConfirmedSafeHeadRequest(result_tx) => {
                if result_tx.send(state.confirmed_safe_head()).is_err() {
                    warn!(
                        target: "derivation",
                        "failed to return confirmed safe head to test observer"
                    );
                }
                Ok(AfterMailbox::Idle(state.into_idle()))
            }
        }
    }

    async fn wait_mailbox<IdleState: MailboxIdle>(
        &mut self,
        state: IdleState,
    ) -> Result<WaitOutcome, DerivationError> {
        let mut recv_timer =
            base_metrics::timed!(Metrics::derivation_actor_inbound_recv_wait_duration_seconds());
        select! {
            biased;

            _ = self.cancellation_token.cancelled() => {
                recv_timer.disarm();
                info!(
                    target: "derivation",
                    "Received shutdown signal. Exiting derivation task."
                );
                Ok(WaitOutcome::Shutdown)
            }
            req = self.inbound_request_rx.recv() => {
                recv_timer.stop();
                let Some(request_type) = req else {
                    error!(target: "derivation", "DerivationActor inbound request receiver closed unexpectedly");
                    self.cancellation_token.cancel();
                    return Err(DerivationError::RequestReceiveFailed);
                };
                Ok(WaitOutcome::Next(self.handle_mailbox(state, request_type).await?))
            }
        }
    }

    async fn wait_safe_head(
        &mut self,
        mut state: AwaitingSafeHead,
    ) -> Result<WaitOutcome, DerivationError> {
        let mut await_confirmation = true;
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
                    return Ok(WaitOutcome::Shutdown);
                }
                confirmed = &mut state.confirmed_rx, if await_confirmation => {
                    recv_timer.stop();
                    let head = match confirmed {
                        Ok(head) => head,
                        Err(_) => {
                            warn!(
                                target: "derivation",
                                "Derived-attribute confirmation sender dropped; waiting on mailbox"
                            );
                            await_confirmation = false;
                            continue;
                        }
                    };
                    info!(target: "derivation", safe_head = ?head, "Received derived-attribute confirmation.");
                    self.record_engine_safe_head(head, true).await?;
                    return Ok(WaitOutcome::Next(AfterMailbox::Derive(
                        state.on_attributes_confirmed(head),
                    )));
                }
                req = self.inbound_request_rx.recv() => {
                    recv_timer.stop();
                    let Some(request_type) = req else {
                        error!(target: "derivation", "DerivationActor inbound request receiver closed unexpectedly");
                        self.cancellation_token.cancel();
                        return Err(DerivationError::RequestReceiveFailed);
                    };
                    let is_safe_head_update = matches!(
                        request_type,
                        DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(_)
                    );
                    match self.handle_mailbox(state, request_type).await? {
                        AfterMailbox::Idle(Idle::SafeHead(next))
                            if !await_confirmation && is_safe_head_update =>
                        {
                            return Ok(WaitOutcome::Next(AfterMailbox::Derive(Deriving::new(
                                next.confirmed_safe_head,
                            ))));
                        }
                        AfterMailbox::Idle(Idle::SafeHead(next)) => {
                            state = next;
                        }
                        other => return Ok(WaitOutcome::Next(other)),
                    }
                }
            }
        }
    }
}

impl Deriving {
    /// Steps the pipeline and returns the next idle wait state.
    pub async fn run<DerivationEngineClient_, PipelineSignalReceiver>(
        self,
        actor: &mut DerivationActor<DerivationEngineClient_, PipelineSignalReceiver>,
    ) -> Result<Idle, DerivationError>
    where
        DerivationEngineClient_: DerivationEngineClient,
        PipelineSignalReceiver: Pipeline + SignalReceiver,
    {
        info!(
            target: "derivation",
            derivation_state = self.confirmed_safe_head.block_info.number,
            "Attempting derivation."
        );

        match actor.produce_next_attributes(self.confirmed_safe_head).await? {
            ProduceAttributes::Ready(payload_attributes) => {
                trace!(target: "derivation", ?payload_attributes, "Produced payload attributes.");
                actor.finalizer.enqueue_for_finalization(&payload_attributes);
                actor.pending_derived_from = payload_attributes.derived_from;
                let (waiting, confirmed_tx) = self.attributes_derived();
                actor
                    .engine_client
                    .send_derived_attributes(*payload_attributes, confirmed_tx)
                    .await
                    .map_err(|e| DerivationError::Sender(Box::new(e)))?;
                Ok(Idle::SafeHead(waiting))
            }
            ProduceAttributes::NeedMoreData => {
                info!(target: "derivation", "Yielding derivation until more data is available.");
                Ok(Idle::L1Data(self.more_data_needed()))
            }
            ProduceAttributes::NeedSignal => Ok(Idle::Signal(self.signal_needed())),
        }
    }
}

impl Idle {
    /// Waits for the next event legal in this idle state.
    pub async fn wait<DerivationEngineClient_, PipelineSignalReceiver>(
        self,
        actor: &mut DerivationActor<DerivationEngineClient_, PipelineSignalReceiver>,
    ) -> Result<WaitOutcome, DerivationError>
    where
        DerivationEngineClient_: DerivationEngineClient,
        PipelineSignalReceiver: Pipeline + SignalReceiver,
    {
        match self {
            Self::ELSync(state) => actor.wait_mailbox(state).await,
            Self::L1Data(state) => actor.wait_mailbox(state).await,
            Self::Signal(state) => actor.wait_mailbox(state).await,
            Self::AfterSignal(state) => actor.wait_mailbox(state).await,
            Self::SafeHead(state) => actor.wait_safe_head(state).await,
        }
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
        let mut idle = Idle::initial();
        loop {
            let after = match idle.wait(&mut self).await? {
                WaitOutcome::Shutdown => return Ok(()),
                WaitOutcome::Next(after) => after,
            };
            idle = match after {
                AfterMailbox::Idle(next) => next,
                AfterMailbox::Derive(deriving) => deriving.run(&mut self).await?,
            };
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
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use alloy_eips::BlockNumHash;
    use alloy_primitives::{B256, BlockHash};
    use async_trait::async_trait;
    use base_common_genesis::{RollupConfig, SystemConfig};
    use base_common_rpc_types_engine::BasePayloadAttributes;
    use base_consensus_derive::{
        OriginProvider, Pipeline, PipelineError, PipelineErrorKind, PipelineResult, ResetError,
        ResetSignal, Signal, SignalReceiver, StepResult,
    };
    use base_consensus_safedb::{DisabledSafeDB, SafeDBError, SafeHeadListener};
    use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};
    use tokio::sync::{mpsc, oneshot, watch};
    use tokio_util::sync::CancellationToken;

    use super::{DerivationActor, DerivationError};
    use crate::{
        DerivationActorRequest, DerivationEngineClient, DerivationState, EngineClientResult,
        NodeActor, ResetReason,
    };

    fn block(number: u64, hash_byte: u8) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::with_last_byte(hash_byte),
                number,
                parent_hash: BlockHash::default(),
                timestamp: number,
            },
            l1_origin: BlockNumHash { hash: BlockHash::default(), number: 0 },
            seq_num: 0,
        }
    }

    fn l1_block(number: u64, hash_byte: u8) -> BlockInfo {
        BlockInfo {
            hash: B256::with_last_byte(hash_byte),
            number,
            parent_hash: BlockHash::default(),
            timestamp: number,
        }
    }

    fn attributes_from(parent: L2BlockInfo, derived_from: BlockInfo) -> AttributesWithParent {
        AttributesWithParent {
            attributes: BasePayloadAttributes::default(),
            parent,
            derived_from: Some(derived_from),
            is_last_in_span: true,
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum StubStep {
        Yield,
        Attributes,
        Reorg,
    }

    #[derive(Debug)]
    struct StubPipeline {
        origin: Option<BlockInfo>,
        next_attrs: VecDeque<AttributesWithParent>,
        step: StubStep,
        cfg: RollupConfig,
    }

    impl StubPipeline {
        fn yielding() -> Self {
            Self {
                origin: None,
                next_attrs: VecDeque::new(),
                step: StubStep::Yield,
                cfg: RollupConfig::default(),
            }
        }

        fn with_attributes() -> Self {
            Self::with_attribute_queue(vec![attributes_from(block(1, 1), l1_block(10, 0xa))])
        }

        fn with_attribute_queue(attrs: Vec<AttributesWithParent>) -> Self {
            Self {
                origin: None,
                next_attrs: attrs.into(),
                step: StubStep::Attributes,
                cfg: RollupConfig::default(),
            }
        }

        fn reorg() -> Self {
            Self {
                origin: None,
                next_attrs: VecDeque::new(),
                step: StubStep::Reorg,
                cfg: RollupConfig::default(),
            }
        }
    }

    impl Iterator for StubPipeline {
        type Item = AttributesWithParent;

        fn next(&mut self) -> Option<Self::Item> {
            self.next_attrs.pop_front()
        }
    }

    impl OriginProvider for StubPipeline {
        fn origin(&self) -> Option<BlockInfo> {
            self.origin
        }
    }

    #[async_trait]
    impl Pipeline for StubPipeline {
        fn peek(&self) -> Option<&AttributesWithParent> {
            self.next_attrs.front()
        }

        async fn step(&mut self, _cursor: L2BlockInfo) -> StepResult {
            if matches!(self.step, StubStep::Attributes) && !self.next_attrs.is_empty() {
                return StepResult::PreparedAttributes;
            }
            match self.step {
                StubStep::Reorg => StepResult::StepFailed(PipelineErrorKind::Reset(
                    ResetError::ReorgDetected(B256::ZERO, B256::with_last_byte(1)),
                )),
                StubStep::Yield | StubStep::Attributes => {
                    StepResult::StepFailed(PipelineError::Eof.temp())
                }
            }
        }

        fn rollup_config(&self) -> &RollupConfig {
            &self.cfg
        }

        async fn system_config_by_number(
            &mut self,
            _number: u64,
        ) -> Result<SystemConfig, PipelineErrorKind> {
            Ok(SystemConfig::default())
        }
    }

    #[async_trait]
    impl SignalReceiver for StubPipeline {
        async fn signal(&mut self, _signal: Signal) -> PipelineResult<()> {
            Ok(())
        }
    }

    #[derive(Debug, Clone, Default)]
    struct RecordingSafeDB {
        updates: Arc<Mutex<Vec<(L2BlockInfo, BlockInfo)>>>,
        resets: Arc<Mutex<Vec<L2BlockInfo>>>,
    }

    #[async_trait]
    impl SafeHeadListener for RecordingSafeDB {
        async fn safe_head_updated(
            &self,
            safe_head: L2BlockInfo,
            l1_block: BlockInfo,
        ) -> Result<(), SafeDBError> {
            self.updates.lock().expect("updates lock").push((safe_head, l1_block));
            Ok(())
        }

        async fn safe_head_reset(&self, reset_safe_head: L2BlockInfo) -> Result<(), SafeDBError> {
            self.resets.lock().expect("resets lock").push(reset_safe_head);
            Ok(())
        }
    }

    #[derive(Clone, Debug, Default)]
    struct FakeEngine {
        confirmed: Arc<Mutex<Option<oneshot::Sender<L2BlockInfo>>>>,
        resets: Arc<Mutex<u64>>,
    }

    #[async_trait]
    impl DerivationEngineClient for FakeEngine {
        async fn reset_engine_forkchoice(&self, _reason: ResetReason) -> EngineClientResult<()> {
            *self.resets.lock().expect("reset lock") += 1;
            Ok(())
        }

        async fn send_finalized_l2_block(&self, _block_number: u64) -> EngineClientResult<()> {
            Ok(())
        }

        async fn send_derived_attributes(
            &self,
            _attributes: AttributesWithParent,
            confirmed: oneshot::Sender<L2BlockInfo>,
        ) -> EngineClientResult<()> {
            *self.confirmed.lock().expect("confirmed lock") = Some(confirmed);
            Ok(())
        }

        async fn send_delegated_safe_head(
            &self,
            _safe_l2: L2BlockInfo,
        ) -> EngineClientResult<()> {
            Ok(())
        }
    }

    async fn current_state(tx: &mpsc::Sender<DerivationActorRequest>) -> DerivationState {
        let (result_tx, result_rx) = oneshot::channel();
        tx.send(DerivationActorRequest::CurrentStateRequest(result_tx))
            .await
            .expect("actor mailbox open");
        result_rx.await.expect("state response")
    }

    async fn current_confirmed_safe_head(tx: &mpsc::Sender<DerivationActorRequest>) -> L2BlockInfo {
        let (result_tx, result_rx) = oneshot::channel();
        tx.send(DerivationActorRequest::CurrentConfirmedSafeHeadRequest(result_tx))
            .await
            .expect("actor mailbox open");
        result_rx.await.expect("confirmed safe head response")
    }

    async fn wait_for_state(
        tx: &mpsc::Sender<DerivationActorRequest>,
        expected: DerivationState,
    ) -> DerivationState {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let state = current_state(tx).await;
            if state == expected || tokio::time::Instant::now() >= deadline {
                return state;
            }
            tokio::task::yield_now().await;
        }
    }

    fn spawn_actor(
        pipeline: StubPipeline,
        engine: FakeEngine,
    ) -> (
        mpsc::Sender<DerivationActorRequest>,
        CancellationToken,
        FakeEngine,
        tokio::task::JoinHandle<Result<(), DerivationError>>,
    ) {
        let (tx, cancel, engine, handle) =
            spawn_actor_with_listener(pipeline, engine, Arc::new(DisabledSafeDB));
        (tx, cancel, engine, handle)
    }

    fn spawn_actor_with_listener(
        pipeline: StubPipeline,
        engine: FakeEngine,
        listener: Arc<dyn SafeHeadListener>,
    ) -> (
        mpsc::Sender<DerivationActorRequest>,
        CancellationToken,
        FakeEngine,
        tokio::task::JoinHandle<Result<(), DerivationError>>,
    ) {
        let cancel = CancellationToken::new();
        let (tx, rx) = mpsc::channel(16);
        let (origin_tx, _origin_rx) = watch::channel(None);
        let actor =
            DerivationActor::new(engine.clone(), cancel.clone(), rx, pipeline, listener, origin_tx);
        let handle = tokio::spawn(actor.start(()));
        (tx, cancel, engine, handle)
    }

    #[tokio::test]
    async fn l1_wait_absorbs_engine_safe_head_and_stays_up() {
        let (tx, cancel, _engine, handle) =
            spawn_actor(StubPipeline::yielding(), FakeEngine::default());

        tx.send(DerivationActorRequest::ProcessEngineSyncCompletionRequest(Box::new(block(1, 1))))
            .await
            .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingL1Data).await,
            DerivationState::AwaitingL1Data
        );

        tx.send(DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(Box::new(block(4, 4))))
            .await
            .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingL1Data).await,
            DerivationState::AwaitingL1Data
        );
        assert_eq!(current_confirmed_safe_head(&tx).await, block(4, 4));
        assert!(!handle.is_finished());

        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn awaiting_safe_head_stays_on_stale_mailbox_until_oneshot() {
        let (tx, cancel, engine, handle) =
            spawn_actor(StubPipeline::with_attributes(), FakeEngine::default());

        tx.send(DerivationActorRequest::ProcessEngineSyncCompletionRequest(Box::new(block(1, 1))))
            .await
            .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingSafeHeadConfirmation).await,
            DerivationState::AwaitingSafeHeadConfirmation
        );

        tx.send(DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(Box::new(block(1, 9))))
            .await
            .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingSafeHeadConfirmation).await,
            DerivationState::AwaitingSafeHeadConfirmation
        );

        let confirmed = engine.confirmed.lock().expect("confirmed lock").take();
        confirmed.expect("oneshot sender").send(block(2, 2)).unwrap();

        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingL1Data).await,
            DerivationState::AwaitingL1Data
        );

        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn leftover_mailbox_does_not_steal_next_inclusion_block() {
        let safedb = RecordingSafeDB::default();
        let pipeline = StubPipeline::with_attribute_queue(vec![
            attributes_from(block(1, 1), l1_block(10, 0xa)),
            attributes_from(block(2, 2), l1_block(20, 0xb)),
        ]);
        let (tx, cancel, engine, handle) =
            spawn_actor_with_listener(pipeline, FakeEngine::default(), Arc::new(safedb.clone()));

        tx.send(DerivationActorRequest::ProcessEngineSyncCompletionRequest(Box::new(block(1, 1))))
            .await
            .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingSafeHeadConfirmation).await,
            DerivationState::AwaitingSafeHeadConfirmation
        );

        let first = engine.confirmed.lock().expect("confirmed lock").take();
        first.expect("first oneshot").send(block(2, 2)).unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingSafeHeadConfirmation).await,
            DerivationState::AwaitingSafeHeadConfirmation
        );

        tx.send(DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(Box::new(block(2, 2))))
            .await
            .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingSafeHeadConfirmation).await,
            DerivationState::AwaitingSafeHeadConfirmation
        );

        let second = engine.confirmed.lock().expect("confirmed lock").take();
        second.expect("second oneshot").send(block(3, 3)).unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingL1Data).await,
            DerivationState::AwaitingL1Data
        );

        let updates = safedb.updates.lock().expect("updates lock").clone();
        let pairs: Vec<(u64, u64)> =
            updates.iter().map(|(l2, l1)| (l2.block_info.number, l1.number)).collect();
        assert!(
            !pairs.contains(&(2, 20)),
            "leftover mailbox stole the next inclusion block: {pairs:?}"
        );
        assert!(pairs.contains(&(2, 10)), "missing first-batch SafeDB pair: {pairs:?}");
        assert!(pairs.contains(&(3, 20)), "missing second-batch SafeDB pair: {pairs:?}");

        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn dropped_oneshot_does_not_kill_actor() {
        let (tx, cancel, engine, handle) =
            spawn_actor(StubPipeline::with_attributes(), FakeEngine::default());
        tx.send(DerivationActorRequest::ProcessEngineSyncCompletionRequest(Box::new(block(1, 1))))
            .await
            .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingSafeHeadConfirmation).await,
            DerivationState::AwaitingSafeHeadConfirmation
        );

        drop(engine.confirmed.lock().expect("confirmed lock").take());
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        tx.send(DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(Box::new(block(3, 3))))
            .await
            .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingL1Data).await,
            DerivationState::AwaitingL1Data
        );
        assert_eq!(current_confirmed_safe_head(&tx).await, block(3, 3));
        assert!(!handle.is_finished());

        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn reset_signal_during_awaiting_safe_head_goes_to_after_signal() {
        let (tx, cancel, _engine, handle) =
            spawn_actor(StubPipeline::with_attributes(), FakeEngine::default());
        tx.send(DerivationActorRequest::ProcessEngineSyncCompletionRequest(Box::new(block(1, 1))))
            .await
            .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingSafeHeadConfirmation).await,
            DerivationState::AwaitingSafeHeadConfirmation
        );

        tx.send(DerivationActorRequest::ProcessEngineSignalRequest(Box::new(
            ResetSignal { l2_safe_head: block(1, 1) }.signal(),
        )))
        .await
        .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingUpdateAfterSignal).await,
            DerivationState::AwaitingUpdateAfterSignal
        );
        assert_eq!(current_confirmed_safe_head(&tx).await, block(1, 1));
        assert!(!handle.is_finished());

        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn reset_signal_rewinds_cursor_before_after_signal_derive() {
        let safedb = Arc::new(RecordingSafeDB::default());
        let (tx, cancel, _engine, handle) = spawn_actor_with_listener(
            StubPipeline::yielding(),
            FakeEngine::default(),
            Arc::clone(&safedb) as Arc<dyn SafeHeadListener>,
        );
        tx.send(DerivationActorRequest::ProcessEngineSyncCompletionRequest(Box::new(block(5, 5))))
            .await
            .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingL1Data).await,
            DerivationState::AwaitingL1Data
        );
        assert_eq!(current_confirmed_safe_head(&tx).await, block(5, 5));

        let reset_head = block(3, 3);
        tx.send(DerivationActorRequest::ProcessEngineSignalRequest(Box::new(
            ResetSignal { l2_safe_head: reset_head }.signal(),
        )))
        .await
        .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingUpdateAfterSignal).await,
            DerivationState::AwaitingUpdateAfterSignal
        );
        assert_eq!(current_confirmed_safe_head(&tx).await, reset_head);
        assert_eq!(
            safedb.resets.lock().expect("resets lock").last(),
            Some(&reset_head),
            "SafeDB reset must use the rewound head, not the pre-reset cursor"
        );

        tx.send(DerivationActorRequest::ProcessL1HeadUpdateRequest(Box::new(l1_block(30, 0x1e))))
            .await
            .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingL1Data).await,
            DerivationState::AwaitingL1Data
        );
        assert_eq!(current_confirmed_safe_head(&tx).await, reset_head);

        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn deriving_run_yield_goes_to_awaiting_l1_data() {
        let (tx, cancel, _engine, handle) =
            spawn_actor(StubPipeline::yielding(), FakeEngine::default());
        tx.send(DerivationActorRequest::ProcessEngineSyncCompletionRequest(Box::new(block(1, 1))))
            .await
            .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingL1Data).await,
            DerivationState::AwaitingL1Data
        );
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn deriving_run_attributes_go_to_awaiting_safe_head() {
        let (tx, cancel, engine, handle) =
            spawn_actor(StubPipeline::with_attributes(), FakeEngine::default());
        tx.send(DerivationActorRequest::ProcessEngineSyncCompletionRequest(Box::new(block(1, 1))))
            .await
            .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingSafeHeadConfirmation).await,
            DerivationState::AwaitingSafeHeadConfirmation
        );
        assert!(engine.confirmed.lock().expect("confirmed lock").is_some());
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn deriving_run_reset_goes_to_awaiting_signal() {
        let (tx, cancel, engine, handle) =
            spawn_actor(StubPipeline::reorg(), FakeEngine::default());
        tx.send(DerivationActorRequest::ProcessEngineSyncCompletionRequest(Box::new(block(1, 1))))
            .await
            .unwrap();
        assert_eq!(
            wait_for_state(&tx, DerivationState::AwaitingSignal).await,
            DerivationState::AwaitingSignal
        );
        assert_eq!(*engine.resets.lock().expect("reset lock"), 1);
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }
}
