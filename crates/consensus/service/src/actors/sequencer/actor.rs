//! The [`SequencerActor`].

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::B256;
use async_trait::async_trait;
use base_consensus_derive::AttributesBuilder;
use base_consensus_genesis::RollupConfig;
use base_consensus_rpc::SequencerAdminAPIError;
use tokio::{
    select,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

use crate::{
    CancellableContext, NodeActor, SequencerAdminQuery, UnsafePayloadGossipClient,
    actors::{
        SequencerEngineClient,
        engine::EngineClientError,
        sequencer::{
            build::{PayloadBuilder, UnsealedPayloadHandle},
            conductor::Conductor,
            error::SequencerActorError,
            metrics::{
                inc_seal_error, inc_seal_pipeline_overlap, update_seal_duration_metrics,
                update_total_transactions_sequenced,
            },
            origin_selector::OriginSelector,
            recovery::RecoveryModeGuard,
            seal::PayloadSealer,
        },
    },
};

/// Sender stashed by `stop_sequencer` when waiting for an in-flight seal pipeline to drain.
pub type PendingStopSender = oneshot::Sender<Result<B256, SequencerAdminAPIError>>;

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
    /// Drives L1 origin selection, attribute preparation, and block build initiation.
    pub builder: PayloadBuilder<AttributesBuilder_, OriginSelector_, SequencerEngineClient_>,
    /// The cancellation token, shared between all tasks.
    pub cancellation_token: CancellationToken,
    /// The optional conductor RPC client.
    pub conductor: Option<Conductor_>,
    /// The struct used to interact with the engine.
    pub engine_client: Arc<SequencerEngineClient_>,
    /// Whether the sequencer is active.
    pub is_active: bool,
    /// Shared recovery mode flag.
    pub recovery_mode: RecoveryModeGuard,
    /// The rollup configuration.
    pub rollup_config: Arc<RollupConfig>,
    /// A client to asynchronously sign and gossip built payloads to the network actor.
    pub unsafe_payload_gossip_client: UnsafePayloadGossipClient_,
    /// In-flight seal pipeline. [`Some`] while a sealed payload is being committed,
    /// gossiped, and inserted. [`None`] when idle.
    pub sealer: Option<PayloadSealer>,
    /// Stashed response sender for a pending `stop_sequencer` call that is waiting
    /// for the in-flight seal pipeline to complete before responding.
    pub pending_stop: Option<PendingStopSender>,
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

    /// Schedules the initial engine reset request and waits for the unsafe head to be updated.
    ///
    /// If the EL is still syncing (snap sync in progress), the engine will defer the reset and
    /// return [`EngineClientError::ELSyncing`]. In that case we wait one block time and retry,
    /// so we never send a `forkchoice_updated` that would abort reth's in-progress EL sync.
    async fn schedule_initial_reset(&self) -> Result<(), SequencerActorError> {
        loop {
            select! {
                biased;
                _ = self.cancellation_token.cancelled() => return Ok(()),
                result = self.engine_client.reset_engine_forkchoice() => match result {
                    Ok(()) => return Ok(()),
                    Err(EngineClientError::ELSyncing) => {
                        info!(target: "sequencer", "EL sync in progress; deferring initial engine reset");
                    }
                    Err(err) => {
                        error!(target: "sequencer", error = ?err, "Failed to send reset request to engine");
                        return Err(err.into());
                    }
                },
            }
            // Wait one block time before retrying, but honour cancellation.
            select! {
                biased;
                _ = self.cancellation_token.cancelled() => return Ok(()),
                _ = tokio::time::sleep(Duration::from_secs(self.rollup_config.block_time)) => {}
            }
        }
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
                            inc_seal_pipeline_overlap();
                            self.sealer = None;
                        }

                        let seal_start = Instant::now();
                        match self.seal_payload(&handle).await {
                            Ok(new_sealer) => {
                                last_seal_duration = seal_start.elapsed();
                                self.sealer = Some(new_sealer);
                            }
                            Err(SequencerActorError::EngineError(EngineClientError::SealError(err))) => {
                                if err.is_fatal() {
                                    error!(target: "sequencer", error = ?err, "Critical seal task error occurred");
                                    inc_seal_error(true);
                                    self.cancellation_token.cancel();
                                    return Err(SequencerActorError::EngineError(EngineClientError::SealError(err)));
                                }
                                warn!(target: "sequencer", error = ?err, "Non-fatal seal error, dropping block");
                                inc_seal_error(false);
                            }
                            Err(other_err) => {
                                error!(target: "sequencer", error = ?other_err, "Unexpected error sealing payload");
                                self.cancellation_token.cancel();
                                return Err(other_err);
                            }
                        }
                    }

                    next_payload_to_seal = self.builder.build().await?;

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
                Some(result) = async {
                    match self.sealer.as_mut() {
                        Some(s) => Some(s.step(
                            &self.conductor,
                            &self.unsafe_payload_gossip_client,
                            &self.engine_client,
                        ).await),
                        None => std::future::pending().await,
                    }
                } => {
                    match result {
                        Ok(true) => {
                            self.sealer = None;
                            if let Some(tx) = self.pending_stop.take() {
                                let result = self.resolve_stop_head().await;
                                if tx.send(result).is_err() {
                                    warn!(target: "sequencer", "Failed to send deferred stop_sequencer response");
                                }
                            }
                        }
                        Ok(false) => {}
                        Err(err) => {
                            let step = self.sealer.as_ref().map(|s| s.state.label()).unwrap_or("unknown");
                            warn!(target: "sequencer", error = ?err, step, "Seal step failed, will retry");
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
