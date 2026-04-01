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
use base_protocol::L2BlockInfo;
use tokio::{
    select,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

use crate::{
    CancellableContext, Metrics, NodeActor, SequencerAdminQuery, UnsafePayloadGossipClient,
    actors::{
        SequencerEngineClient,
        engine::EngineClientError,
        sequencer::{
            build::{PayloadBuilder, UnsealedPayloadHandle},
            conductor::Conductor,
            error::SequencerActorError,
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
    /// Expected [`L2BlockInfo`] parent for the next build.
    ///
    /// Set in the ticker arm when a seal succeeds (derived from the sealed envelope). Consumed
    /// in the `Ok(true)` sealer arm via [`PayloadBuilder::build_on`], which is called after
    /// `insert_unsafe_payload` has already been fire-and-forgot to the engine. This ordering
    /// guarantees the engine's `InsertTask` is queued before `BuildTask`, so the EL always
    /// builds on the correct (just-inserted) parent instead of the stale watch value.
    pub next_build_parent: Option<L2BlockInfo>,
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

        Metrics::sequencer_block_building_seal_task_duration().set(seal_request_start.elapsed());
        Metrics::sequencer_total_transactions_sequenced()
            .increment(handle.attributes_with_parent.count_transactions());

        Ok(PayloadSealer::new(envelope))
    }

    /// Attempts to seal a pre-built payload, first checking whether it is still fresh.
    ///
    /// If the unsafe head has advanced past the handle's parent since build time (a P2P block
    /// arrived while the build was in-flight), the handle is discarded and `Ok(None)` is
    /// returned so the caller can restart with [`PayloadBuilder::build`].
    ///
    /// On success returns the new [`PayloadSealer`] together with the elapsed seal duration so
    /// the caller can reschedule the ticker accurately. On a non-fatal seal error returns
    /// `Ok(None)`. On a fatal error the cancellation token is triggered and `Err` is returned.
    pub(super) async fn try_seal_handle(
        &self,
        handle: UnsealedPayloadHandle,
    ) -> Result<Option<(PayloadSealer, Duration)>, SequencerActorError> {
        let current_head = self.engine_client.get_unsafe_head().await?;
        let build_parent = handle.attributes_with_parent.parent().block_info;

        if current_head.block_info.number > build_parent.number {
            warn!(
                target: "sequencer",
                parent_num = build_parent.number,
                current_head_num = current_head.block_info.number,
                "Stale build detected: unsafe head advanced past build parent, discarding"
            );
            Metrics::sequencer_stale_build_discarded_total().increment(1);
            return Ok(None);
        }

        if current_head.block_info.number == build_parent.number
            && current_head.block_info.hash != build_parent.hash
        {
            warn!(
                target: "sequencer",
                parent_num = build_parent.number,
                expected_hash = %build_parent.hash,
                actual_hash = %current_head.block_info.hash,
                "Stale build detected: unsafe head reorged at same height, discarding"
            );
            Metrics::sequencer_stale_build_discarded_total().increment(1);
            return Ok(None);
        }

        // Staleness check above is best-effort: if the unsafe head advances between the
        // get_unsafe_head() call and seal_payload() below, the EL's own validation is
        // the final safety gate.
        let seal_start = Instant::now();
        match self.seal_payload(&handle).await {
            Ok(sealer) => Ok(Some((sealer, seal_start.elapsed()))),
            Err(SequencerActorError::EngineError(EngineClientError::SealError(err))) => {
                if err.is_fatal() {
                    error!(target: "sequencer", error = ?err, "Critical seal task error occurred");
                    Metrics::sequencer_seal_errors_total("true").increment(1);
                    self.cancellation_token.cancel();
                    return Err(SequencerActorError::EngineError(EngineClientError::SealError(
                        err,
                    )));
                }
                warn!(target: "sequencer", error = ?err, "Non-fatal seal error, dropping block");
                Metrics::sequencer_seal_errors_total("false").increment(1);
                Ok(None)
            }
            Err(other_err) => {
                error!(target: "sequencer", error = ?other_err, "Unexpected error sealing payload");
                self.cancellation_token.cancel();
                Err(other_err)
            }
        }
    }

    /// Schedules the initial engine reset request and waits for the unsafe head to be updated.
    ///
    /// If the EL is still syncing (snap sync in progress), the engine will defer the reset and
    /// return [`EngineClientError::ELSyncing`]. In that case we wait one block time and retry,
    /// so we never send a `forkchoice_updated` that would abort reth's in-progress EL sync.
    ///
    /// Admin API queries are serviced throughout — both during reset attempts and during the
    /// backoff sleep — so that control can reach the sequencer while EL sync is in progress.
    async fn schedule_initial_reset(
        &mut self,
        next_payload: &mut Option<UnsealedPayloadHandle>,
    ) -> Result<(), SequencerActorError> {
        loop {
            select! {
                biased;
                _ = self.cancellation_token.cancelled() => return Ok(()),
                Some(query) = self.admin_api_rx.recv() => {
                    self.handle_admin_query(next_payload, query).await;
                }
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
            // Wait one block time before retrying the reset, but service admin queries
            // and honour cancellation throughout the backoff window.
            let sleep = tokio::time::sleep(Duration::from_secs(self.rollup_config.block_time));
            tokio::pin!(sleep);
            loop {
                select! {
                    biased;
                    _ = self.cancellation_token.cancelled() => return Ok(()),
                    Some(query) = self.admin_api_rx.recv() => {
                        self.handle_admin_query(next_payload, query).await;
                    }
                    _ = &mut sleep => break,
                }
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

        let mut next_payload_to_seal: Option<UnsealedPayloadHandle> = None;

        // Reset the engine state prior to beginning block building.
        // Admin API queries are serviced during this phase (see schedule_initial_reset).
        self.schedule_initial_reset(&mut next_payload_to_seal).await?;
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

                    self.handle_admin_query(&mut next_payload_to_seal, query).await;

                    if !active_before && self.is_active {
                        build_ticker.reset_immediately();
                    }
                }
                // Drive the seal pipeline (commit → gossip → insert) one step per iteration.
                // The ticker arm is gated on `sealer.is_none()` so the two are mutually
                // exclusive — when a seal is in-flight the ticker cannot fire and interrupt it.
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
                            // Respond to a pending stop_sequencer request now that the
                            // in-flight seal is complete.
                            if let Some(tx) = self.pending_stop.take() {
                                let result = self.resolve_stop_head().await;
                                if tx.send(result).is_err() {
                                    warn!(target: "sequencer", "Failed to send deferred stop_sequencer response");
                                }
                            }
                            // Build the next payload on the correct parent now that
                            // insert_unsafe_payload has already been fire-and-forgot to the engine.
                            // next_build_parent was computed from the sealed envelope in the ticker
                            // arm; using it here ensures InsertTask is enqueued before BuildTask so
                            // the EL builds on the just-inserted block instead of its grandparent.
                            if self.is_active {
                                next_payload_to_seal =
                                    if let Some(parent) = self.next_build_parent.take() {
                                        let result = self.builder.build_on(parent).await?;
                                        // If the build returned None (the just-inserted parent block
                                        // is not yet indexed by the L2 provider — insert_unsafe_payload
                                        // is fire-and-forgot), restore next_build_parent so the
                                        // immediate ticker retry uses build_on with the known correct
                                        // parent rather than the potentially stale watch head, which
                                        // could cause the wrong block to be built.
                                        if result.is_none() {
                                            self.next_build_parent = Some(parent);
                                            build_ticker.reset_immediately();
                                        }
                                        result
                                    } else {
                                        self.builder.build().await?
                                    };
                            }
                        }
                        Ok(false) => {}
                        Err(err) => {
                            let step = self.sealer.as_ref().map(|s| s.state.label()).unwrap_or("unknown");
                            warn!(target: "sequencer", error = ?err, step, "Seal step failed, will retry");
                        }
                    }
                }
                // Tick is gated on `self.sealer.is_none()` to make the ticker and sealer arms
                // mutually exclusive. In catch-up mode reset_immediately() fires every tick,
                // making the ticker Poll::Ready at the same time as the sealer's step().await
                // is Poll::Pending. Disabling the ticker while a seal is in-flight lets the
                // sealer arm complete all three steps (commit → gossip → insert) before the
                // next block starts, so the canonical head actually advances.
                _ = build_ticker.tick(), if self.is_active && self.sealer.is_none() => {
                    if let Some(handle) = next_payload_to_seal.take() {
                        // Extract data needed after try_seal_handle consumes the handle.
                        let parent_beacon_root = handle
                            .attributes_with_parent
                            .attributes()
                            .payload_attributes
                            .parent_beacon_block_root;
                        let handle_timestamp = handle
                            .attributes_with_parent
                            .attributes()
                            .payload_attributes
                            .timestamp;
                        match self.try_seal_handle(handle).await? {
                            Some((new_sealer, dur)) => {
                                last_seal_duration = dur;
                                // Stash the expected parent for the next build. This is consumed
                                // in the Ok(true) arm after insert_unsafe_payload is queued,
                                // ensuring BuildTask is enqueued after InsertTask in the engine.
                                self.next_build_parent = match L2BlockInfo::from_payload_and_genesis(
                                    new_sealer.envelope.execution_payload.clone(),
                                    parent_beacon_root,
                                    &self.rollup_config.genesis,
                                ) {
                                    Ok(parent) => Some(parent),
                                    Err(err) => {
                                        warn!(
                                            target: "sequencer",
                                            error = ?err,
                                            "Failed to derive L2BlockInfo from sealed payload; \
                                             next build will fall back to unsafe head watch channel"
                                        );
                                        None
                                    }
                                };
                                self.sealer = Some(new_sealer);
                                // Schedule the next tick for the next block's target seal time.
                                // Use the just-sealed block's timestamp; the next block's
                                // timestamp is one block_time later.
                                let next_block_seconds =
                                    handle_timestamp.saturating_add(self.rollup_config.block_time);
                                let next_block_time = UNIX_EPOCH
                                    + Duration::from_secs(next_block_seconds)
                                    - last_seal_duration;
                                match next_block_time.duration_since(SystemTime::now()) {
                                    Ok(duration) => build_ticker.reset_after(duration),
                                    Err(_) => build_ticker.reset_immediately(),
                                }
                                // Do not call build() here. The next payload is built in the
                                // Ok(true) arm after insert_unsafe_payload has been queued,
                                // so InsertTask always precedes BuildTask in the engine queue.
                            }
                            None => {
                                // Stale build or non-fatal seal error: rebuild immediately on
                                // the current unsafe head.
                                next_payload_to_seal = self.builder.build().await?;
                                if let Some(ref payload) = next_payload_to_seal {
                                    let next_block_seconds = payload
                                        .attributes_with_parent
                                        .parent()
                                        .block_info
                                        .timestamp
                                        .saturating_add(self.rollup_config.block_time);
                                    let next_block_time = UNIX_EPOCH
                                        + Duration::from_secs(next_block_seconds)
                                        - last_seal_duration;
                                    match next_block_time.duration_since(SystemTime::now()) {
                                        Ok(duration) => build_ticker.reset_after(duration),
                                        Err(_) => build_ticker.reset_immediately(),
                                    }
                                } else {
                                    build_ticker.reset_immediately();
                                }
                            }
                        }
                    } else {
                        // No pre-built payload: bootstrap on first tick, or retry after the
                        // Ok(true) arm's build_on failed due to the parent block not yet being
                        // indexed (insert_unsafe_payload is fire-and-forgot). If next_build_parent
                        // is set, use build_on with the known correct parent rather than reading
                        // the potentially stale watch head, which could cause the wrong block to
                        // be built. On failure restore next_build_parent and reset_immediately so
                        // we retry as soon as the engine indexes the block.
                        next_payload_to_seal = if let Some(parent) = self.next_build_parent.take() {
                            let result = self.builder.build_on(parent).await?;
                            if result.is_none() {
                                self.next_build_parent = Some(parent);
                            }
                            result
                        } else {
                            self.builder.build().await?
                        };
                        if let Some(ref payload) = next_payload_to_seal {
                            let next_block_seconds = payload
                                .attributes_with_parent
                                .parent()
                                .block_info
                                .timestamp
                                .saturating_add(self.rollup_config.block_time);
                            let next_block_time = UNIX_EPOCH
                                + Duration::from_secs(next_block_seconds)
                                - last_seal_duration;
                            match next_block_time.duration_since(SystemTime::now()) {
                                Ok(duration) => build_ticker.reset_after(duration),
                                Err(_) => build_ticker.reset_immediately(),
                            }
                        } else {
                            build_ticker.reset_immediately();
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
