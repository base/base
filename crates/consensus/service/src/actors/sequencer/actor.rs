//! The [`SequencerActor`].

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_rpc_types_engine::PayloadId;
use async_trait::async_trait;
use base_consensus_derive::{AttributesBuilder, PipelineErrorKind};
use base_consensus_engine::{InsertTaskError, SealTaskError, SynchronizeTaskError};
use base_consensus_genesis::RollupConfig;
use base_protocol::{BlockInfo, L2BlockInfo, OpAttributesWithParent};
use tokio::{select, sync::mpsc};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

use crate::{
    CancellableContext, NodeActor, PoolActivation, SequencerAdminQuery, UnsafePayloadGossipClient,
    UpgradeActivations,
    actors::{
        SequencerEngineClient,
        engine::EngineClientError,
        sequencer::{
            conductor::Conductor,
            error::SequencerActorError,
            metrics::{
                update_attributes_build_duration_metrics, update_block_build_duration_metrics,
                update_seal_duration_metrics, update_total_transactions_sequenced,
            },
            origin_selector::OriginSelector,
            seal::PayloadSealer,
        },
    },
};

/// The handle to a block that has been started but not sealed.
#[derive(Debug)]
pub(super) struct UnsealedPayloadHandle {
    /// The [`PayloadId`] of the unsealed payload.
    pub payload_id: PayloadId,
    /// The [`OpAttributesWithParent`] used to start block building.
    pub attributes_with_parent: OpAttributesWithParent,
}

/// The [`SequencerActor`] is responsible for building L2 blocks on top of the current unsafe head
/// and scheduling them to be signed and gossipped by the P2P layer, extending the L2 chain with new
/// blocks.
#[derive(Debug)]
pub struct SequencerActor<
    AttributesBuilder_,
    Conductor_,
    OriginSelector_,
    SequencerEngineClient_,
    UnsafePayloadGossipClient_,
> where
    AttributesBuilder_: AttributesBuilder,
    Conductor_: Conductor,
    OriginSelector_: OriginSelector,
    SequencerEngineClient_: SequencerEngineClient,
    UnsafePayloadGossipClient_: UnsafePayloadGossipClient,
{
    /// Receiver for admin API requests.
    pub admin_api_rx: mpsc::Receiver<SequencerAdminQuery>,
    /// The attributes builder used for block building.
    pub attributes_builder: AttributesBuilder_,
    /// The cancellation token, shared between all tasks.
    pub cancellation_token: CancellationToken,
    /// The optional conductor RPC client.
    pub conductor: Option<Conductor_>,
    /// The struct used to interact with the engine.
    pub engine_client: SequencerEngineClient_,
    /// Whether the sequencer is active.
    pub is_active: bool,
    /// Whether the sequencer is in recovery mode.
    pub in_recovery_mode: bool,
    /// The struct used to determine the next L1 origin.
    pub origin_selector: OriginSelector_,
    /// The rollup configuration.
    pub rollup_config: Arc<RollupConfig>,
    /// A client to asynchronously sign and gossip built payloads to the network actor.
    pub unsafe_payload_gossip_client: UnsafePayloadGossipClient_,
    /// In-flight seal pipeline. [`Some`] while a sealed payload is being committed,
    /// gossiped, and inserted. [`None`] when idle.
    pub sealer: Option<PayloadSealer>,
}

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
    /// Fetches the sealed payload envelope from the engine for the given unsealed handle.
    pub(super) async fn seal_payload(
        &self,
        handle: &UnsealedPayloadHandle,
    ) -> Result<PayloadSealer, SequencerActorError> {
        let seal_request_start = Instant::now();

        let envelope = self
            .engine_client
            .get_sealed_payload(handle.payload_id, handle.attributes_with_parent.clone())
            .await?;

        update_seal_duration_metrics(seal_request_start.elapsed());
        update_total_transactions_sequenced(handle.attributes_with_parent.count_transactions());

        Ok(PayloadSealer::new(envelope))
    }

    /// Starts building an L2 block by creating and populating payload attributes referencing the
    /// correct L1 origin block and sending them to the block engine.
    pub(super) async fn build_unsealed_payload(
        &mut self,
    ) -> Result<Option<UnsealedPayloadHandle>, SequencerActorError> {
        let unsafe_head = self.engine_client.get_unsafe_head().await?;

        let Some(l1_origin) = self.get_next_payload_l1_origin(unsafe_head).await? else {
            // Temporary error - retry on next tick.
            return Ok(None);
        };

        info!(
            target: "sequencer",
            parent_num = unsafe_head.block_info.number,
            l1_origin_num = l1_origin.number,
            "Started sequencing new block"
        );

        // Build the payload attributes for the next block.
        let attributes_build_start = Instant::now();

        let Some(attributes_with_parent) = self.build_attributes(unsafe_head, l1_origin).await?
        else {
            // Temporary error or reset - retry on next tick.
            return Ok(None);
        };

        update_attributes_build_duration_metrics(attributes_build_start.elapsed());

        // Send the built attributes to the engine to be built.
        let build_request_start = Instant::now();

        let payload_id =
            self.engine_client.start_build_block(attributes_with_parent.clone()).await?;

        update_block_build_duration_metrics(build_request_start.elapsed());

        Ok(Some(UnsealedPayloadHandle { payload_id, attributes_with_parent }))
    }

    /// Determines and validates the L1 origin block for the provided L2 unsafe head.
    /// Returns `Ok(None)` for temporary errors that should be retried.
    async fn get_next_payload_l1_origin(
        &mut self,
        unsafe_head: L2BlockInfo,
    ) -> Result<Option<BlockInfo>, SequencerActorError> {
        let l1_origin = match self
            .origin_selector
            .next_l1_origin(unsafe_head, self.in_recovery_mode)
            .await
        {
            Ok(l1_origin) => l1_origin,
            Err(err) => {
                warn!(
                    target: "sequencer",
                    ?err,
                    "Temporary error occurred while selecting next L1 origin. Re-attempting on next tick."
                );
                return Ok(None);
            }
        };

        if unsafe_head.l1_origin.hash != l1_origin.parent_hash
            && unsafe_head.l1_origin.hash != l1_origin.hash
        {
            warn!(
                target: "sequencer",
                l1_origin = ?l1_origin,
                unsafe_head_hash = %unsafe_head.l1_origin.hash,
                unsafe_head_l1_origin = ?unsafe_head.l1_origin,
                "Cannot build new L2 block on inconsistent L1 origin, resetting engine"
            );
            self.engine_client.reset_engine_forkchoice().await?;
            return Ok(None);
        }
        Ok(Some(l1_origin))
    }

    /// Builds the `OpAttributesWithParent` for the next block to build. If None is returned, it
    /// indicates that no attributes could be built at this time but future attempts may be made.
    async fn build_attributes(
        &mut self,
        unsafe_head: L2BlockInfo,
        l1_origin: BlockInfo,
    ) -> Result<Option<OpAttributesWithParent>, SequencerActorError> {
        let mut attributes = match self
            .attributes_builder
            .prepare_payload_attributes(unsafe_head, l1_origin.id())
            .await
        {
            Ok(attrs) => attrs,
            Err(PipelineErrorKind::Temporary(_)) => {
                // Temporary error - retry on next tick.
                return Ok(None);
            }
            Err(PipelineErrorKind::Reset(_)) => {
                if let Err(err) = self.engine_client.reset_engine_forkchoice().await {
                    error!(target: "sequencer", ?err, "Failed to reset engine");
                    return Err(SequencerActorError::ChannelClosed);
                }

                warn!(
                    target: "sequencer",
                    "Resetting engine due to pipeline error while preparing payload attributes"
                );
                return Ok(None);
            }
            Err(err @ PipelineErrorKind::Critical(_)) => {
                error!(target: "sequencer", ?err, "Failed to prepare payload attributes");
                return Err(err.into());
            }
        };

        UpgradeActivations::log(&self.rollup_config, &attributes);
        let activator = PoolActivation::new(Arc::clone(&self.rollup_config));
        attributes.no_tx_pool =
            Some(!activator.is_enabled(self.in_recovery_mode, l1_origin, &attributes));

        let attrs_with_parent = OpAttributesWithParent::new(attributes, unsafe_head, None, false);
        Ok(Some(attrs_with_parent))
    }

    /// Schedules the initial engine reset request and waits for the unsafe head to be updated.
    async fn schedule_initial_reset(&self) -> Result<(), SequencerActorError> {
        // Reset the engine, in order to initialize the engine state.
        // NB: this call waits for confirmation that the reset succeeded and we can proceed with
        // post-reset logic.
        self.engine_client.reset_engine_forkchoice().await.map_err(|err| {
            error!(target: "sequencer", ?err, "Failed to send reset request to engine");
            err.into()
        })
    }
}

#[async_trait]
impl<
    AttributesBuilder_,
    Conductor_,
    OriginSelector_,
    SequencerEngineClient_,
    UnsafePayloadGossipClient_,
> NodeActor
    for SequencerActor<
        AttributesBuilder_,
        Conductor_,
        OriginSelector_,
        SequencerEngineClient_,
        UnsafePayloadGossipClient_,
    >
where
    AttributesBuilder_: AttributesBuilder + Sync + 'static,
    Conductor_: Conductor + Sync + 'static,
    OriginSelector_: OriginSelector + Sync + 'static,
    SequencerEngineClient_: SequencerEngineClient + Sync + 'static,
    UnsafePayloadGossipClient_: UnsafePayloadGossipClient + Sync + 'static,
{
    type Error = SequencerActorError;
    type StartData = ();

    async fn start(mut self, _: Self::StartData) -> Result<(), Self::Error> {
        let mut build_ticker =
            tokio::time::interval(Duration::from_secs(self.rollup_config.block_time));

        self.update_metrics();

        // Reset the engine state prior to beginning block building.
        self.schedule_initial_reset().await?;

        let mut next_payload_to_seal: Option<UnsealedPayloadHandle> = None;
        let mut last_seal_duration = Duration::from_secs(0);
        loop {
            select! {
                biased;
                _ = self.cancellation_token.cancelled() => {
                    info!(target: "sequencer", "Received shutdown signal. Exiting sequencer task.");
                    return Ok(());
                }
                Some(query) = self.admin_api_rx.recv() => {
                    let active_before = self.is_active;

                    self.handle_admin_query(query).await;

                    if !active_before && self.is_active {
                        build_ticker.reset_immediately();
                    }
                }
                _ = build_ticker.tick(), if self.is_active => {
                    if let Some(handle) = next_payload_to_seal.take() {
                        if self.sealer.is_some() {
                            error!(target: "sequencer", "Seal pipeline did not complete before next block was sealed");
                            self.sealer = None;
                        }

                        let seal_start = Instant::now();
                        match self.seal_payload(&handle).await {
                            Ok(new_sealer) => {
                                last_seal_duration = seal_start.elapsed();
                                self.sealer = Some(new_sealer);
                            }
                            Err(SequencerActorError::EngineError(EngineClientError::SealError(err))) => {
                                if is_seal_task_err_fatal(&err) {
                                    error!(target: "sequencer", error = ?err, "Critical seal task error occurred");
                                    self.cancellation_token.cancel();
                                    return Err(SequencerActorError::EngineError(EngineClientError::SealError(err)));
                                }
                            }
                            Err(other_err) => {
                                error!(target: "sequencer", error = ?other_err, "Unexpected error sealing payload");
                                self.cancellation_token.cancel();
                                return Err(other_err);
                            }
                        }
                    }

                    next_payload_to_seal = self.build_unsealed_payload().await?;

                    if let Some(ref payload) = next_payload_to_seal {
                        let next_block_seconds = payload.attributes_with_parent.parent().block_info.timestamp.saturating_add(self.rollup_config.block_time);
                        let next_block_time = UNIX_EPOCH + Duration::from_secs(next_block_seconds) - last_seal_duration;
                        match next_block_time.duration_since(SystemTime::now()) {
                            Ok(duration) => build_ticker.reset_after(duration),
                            Err(_) => build_ticker.reset_immediately(),
                        };
                    } else {
                        build_ticker.reset_immediately();
                    }
                }
                // Drive the seal pipeline (commit → gossip → insert) one step at a time.
                result = self.sealer.as_mut().unwrap().step(
                    &self.conductor,
                    &self.unsafe_payload_gossip_client,
                    &self.engine_client,
                ), if self.sealer.is_some() => {
                    match result {
                        Ok(true) => {
                            self.sealer = None;
                        }
                        Ok(false) => {}
                        Err(err) => {
                            warn!(target: "sequencer", error = ?err, "Seal step failed, will retry");
                        }
                    }
                }
            }
        }
    }
}

impl<
    AttributesBuilder_,
    Conductor_,
    OriginSelector_,
    SequencerEngineClient_,
    UnsafePayloadGossipClient_,
> CancellableContext
    for SequencerActor<
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
    fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.cancellation_token.cancelled()
    }
}

// Determines whether the provided [`SealTaskError`] is fatal for the sequencer.
//
// NB: We could use `err.severity()`, but that gives EngineActor control over this classification.
// `SequencerActor` may have different interpretations of severity, and it is not clear when making
// a change in that area of the codebase that it will affect this area. When a new task error is
// added, this approach guarantees compilation will fail until it is handled here.
fn is_seal_task_err_fatal(err: &SealTaskError) -> bool {
    match err {
        SealTaskError::PayloadInsertionFailed(insert_err) => match &**insert_err {
            InsertTaskError::ForkchoiceUpdateFailed(synchronize_error) => match synchronize_error {
                SynchronizeTaskError::FinalizedAheadOfUnsafe(_, _) => true,
                SynchronizeTaskError::ForkchoiceUpdateFailed(_)
                | SynchronizeTaskError::InvalidForkchoiceState
                | SynchronizeTaskError::UnexpectedPayloadStatus(_) => false,
            },
            InsertTaskError::FromBlockError(_) | InsertTaskError::L2BlockInfoConstruction(_) => {
                true
            }
            InsertTaskError::InsertFailed(_) | InsertTaskError::UnexpectedPayloadStatus(_) => false,
        },
        SealTaskError::GetPayloadFailed(_)
        | SealTaskError::HoloceneInvalidFlush
        | SealTaskError::UnsafeHeadChangedSinceBuild => false,
        SealTaskError::DepositOnlyPayloadFailed
        | SealTaskError::DepositOnlyPayloadReattemptFailed
        | SealTaskError::FromBlock(_)
        | SealTaskError::MpscSend(_)
        | SealTaskError::ClockWentBackwards => true,
    }
}
