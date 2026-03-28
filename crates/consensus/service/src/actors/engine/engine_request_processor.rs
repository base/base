use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use base_alloy_rpc_types_engine::OpExecutionPayloadEnvelope;
use base_consensus_derive::{ResetSignal, Signal};
use base_consensus_engine::{
    BuildTask, ConsolidateInput, ConsolidateTask, Engine, EngineClient, EngineSyncStateUpdate,
    EngineTask, EngineTaskError, EngineTaskErrorSeverity, FinalizeTask, GetPayloadTask, InsertTask,
    SealTask,
};
use base_consensus_genesis::RollupConfig;
use base_protocol::L2BlockInfo;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

use crate::{
    BuildRequest, EngineClientError, EngineDerivationClient, EngineError, GetPayloadRequest,
    ResetRequest, SealRequest,
};

/// Requires that the implementor handles [`EngineProcessingRequest`]s via the provided channel.
/// Note: this exists to facilitate unit testing rather than consolidate multiple implementations
/// under a well-thought-out interface.
pub trait EngineRequestReceiver: Send + Sync {
    /// Starts a task to handle engine processing requests.
    fn start(
        self,
        request_channel: mpsc::Receiver<EngineProcessingRequest>,
    ) -> JoinHandle<Result<(), EngineError>>;
}

/// A request to process engine tasks.
#[derive(Debug)]
pub enum EngineProcessingRequest {
    /// Request to start building a block.
    Build(Box<BuildRequest>),
    /// Request to fetch a sealed payload without inserting it.
    GetPayload(Box<GetPayloadRequest>),
    /// Request to process a Safe signal, which can be derived attributes or delegated block info.
    ProcessSafeL2Signal(ConsolidateInput),
    /// Request to process the finalized L2 block with the provided block number.
    ProcessFinalizedL2BlockNumber(Box<u64>),
    /// Request to process a received unsafe L2 block.
    ProcessUnsafeL2Block(Box<OpExecutionPayloadEnvelope>),
    /// Request to reset the forkchoice.
    Reset(Box<ResetRequest>),
    /// Request to seal a block.
    Seal(Box<SealRequest>),
}

/// Responsible for managing the operations sent to the execution layer's Engine API. To accomplish
/// this, it uses the [`Engine`] task queue to order Engine API  interactions based off of
/// the [`Ord`] implementation of [`EngineTask`].
#[derive(Debug)]
pub struct EngineProcessor<EngineClient_, DerivationClient>
where
    EngineClient_: EngineClient,
    DerivationClient: EngineDerivationClient,
{
    /// The client used to send messages to the [`crate::DerivationActor`].
    derivation_client: DerivationClient,
    /// Whether the EL sync is complete. This should only ever go from false to true.
    el_sync_complete: bool,
    /// The last safe head update sent.
    last_safe_head_sent: L2BlockInfo,
    /// The [`RollupConfig`] .
    /// A channel to use to relay the current unsafe head.
    /// ## Note
    /// This is `Some` when the node is in sequencer mode, and `None` when the node is in validator
    /// mode.
    unsafe_head_tx: Option<watch::Sender<L2BlockInfo>>,

    /// The [`RollupConfig`] used to build tasks.
    rollup: Arc<RollupConfig>,
    /// An [`EngineClient`] used for creating engine tasks.
    client: Arc<EngineClient_>,
    /// The [`Engine`] task queue.
    engine: Engine<EngineClient_>,
}

impl<EngineClient_, DerivationClient> EngineProcessor<EngineClient_, DerivationClient>
where
    EngineClient_: EngineClient + 'static,
    DerivationClient: EngineDerivationClient + 'static,
{
    /// Constructs a new [`EngineProcessor`] from the params.
    pub fn new(
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        derivation_client: DerivationClient,
        engine: Engine<EngineClient_>,
        unsafe_head_tx: Option<watch::Sender<L2BlockInfo>>,
    ) -> Self {
        Self {
            client,
            derivation_client,
            el_sync_complete: false,
            engine,
            last_safe_head_sent: L2BlockInfo::default(),
            rollup: config,
            unsafe_head_tx,
        }
    }

    /// Resets the inner [`Engine`] and propagates the reset to the derivation actor.
    async fn reset(&mut self) -> Result<(), EngineError> {
        // Reset the engine.
        let (l2_safe_head, l1_origin, system_config) =
            self.engine.reset(Arc::clone(&self.client), Arc::clone(&self.rollup)).await?;

        // Signal the derivation actor to reset.
        let signal = ResetSignal { l2_safe_head, l1_origin, system_config: Some(system_config) };
        match self.derivation_client.send_signal(signal.signal()).await {
            Ok(_) => info!(target: "engine", "Sent reset signal to derivation actor"),
            Err(err) => {
                error!(target: "engine", ?err, "Failed to send reset signal to the derivation actor");
                return Err(EngineError::ChannelClosed);
            }
        }

        self.send_derivation_actor_safe_head_if_updated().await?;

        Ok(())
    }

    /// Drains the inner [`Engine`] task queue and attempts to update the safe head.
    async fn drain(&mut self) -> Result<(), EngineError> {
        match self.engine.drain().await {
            Ok(_) => {
                trace!(target: "engine", "[ENGINE] tasks drained");
            }
            Err(err) => {
                match err.severity() {
                    EngineTaskErrorSeverity::Critical => {
                        error!(target: "engine", ?err, "Critical error draining engine tasks");
                        return Err(err.into());
                    }
                    EngineTaskErrorSeverity::Reset => {
                        warn!(target: "engine", ?err, "Received reset request");
                        self.reset().await?;
                    }
                    EngineTaskErrorSeverity::Flush => {
                        // This error is encountered when the payload is marked INVALID
                        // by the engine api. Post-holocene, the payload is replaced by
                        // a "deposits-only" block and re-executed. At the same time,
                        // the channel and any remaining buffered batches are flushed.
                        warn!(target: "engine", ?err, "Invalid payload, Flushing derivation pipeline.");
                        match self.derivation_client.send_signal(Signal::FlushChannel).await {
                            Ok(_) => {
                                debug!(target: "engine", "Sent flush signal to derivation actor")
                            }
                            Err(err) => {
                                error!(target: "engine", ?err, "Failed to send flush signal to the derivation actor.");
                                return Err(EngineError::ChannelClosed);
                            }
                        }
                    }
                    EngineTaskErrorSeverity::Temporary => {
                        trace!(target: "engine", ?err, "Temporary error draining engine tasks");
                    }
                }
            }
        }

        self.send_derivation_actor_safe_head_if_updated().await?;

        if !self.el_sync_complete && self.engine.state().el_sync_finished {
            self.mark_el_sync_complete_and_notify_derivation_actor().await?;
        }

        Ok(())
    }

    async fn mark_el_sync_complete_and_notify_derivation_actor(
        &mut self,
    ) -> Result<(), EngineError> {
        self.el_sync_complete = true;

        // Reset the engine if the sync state does not already know about a finalized block.
        if self.engine.state().sync_state.finalized_head() == L2BlockInfo::default() {
            // If the sync status is finished, we can reset the engine and start derivation.
            info!(target: "engine", "Performing initial engine reset");
            self.reset().await?;
        } else {
            info!(target: "engine", "finalized head is not default, so not resetting");
        }

        self.derivation_client
            .notify_sync_completed(self.engine.state().sync_state.safe_head())
            .await
            .map(|_| Ok(()))
            .map_err(|e| {
                error!(target: "engine", ?e, "Failed to notify sync completed");
                EngineError::ChannelClosed
            })?
    }

    /// Attempts to send the [`crate::DerivationActor`] the safe head if updated.
    async fn send_derivation_actor_safe_head_if_updated(&mut self) -> Result<(), EngineError> {
        let engine_safe_head = self.engine.state().sync_state.safe_head();
        if engine_safe_head == self.last_safe_head_sent {
            info!(target: "engine", safe_head = engine_safe_head.block_info.number, "Safe head unchanged");
            debug!(target: "engine", safe_head = ?engine_safe_head, "unchanged safe head");
            // This was already sent, so do not send it.
            return Ok(());
        }

        self.derivation_client.send_new_engine_safe_head(engine_safe_head).await.map_err(|e| {
            error!(target: "engine", ?e, "Failed to send new engine safe head");
            EngineError::ChannelClosed
        })?;

        info!(target: "engine", safe_head = engine_safe_head.block_info.number, "Attempted L2 Safe Head Update");
        debug!(target: "engine", safe_head = ?engine_safe_head, "Attempted L2 Safe Head Update");
        self.last_safe_head_sent = engine_safe_head;

        Ok(())
    }

    fn log_follower_upgrade_activation(&self, envelope: &OpExecutionPayloadEnvelope) {
        if self.unsafe_head_tx.is_some() {
            return;
        }

        self.rollup.log_upgrade_activation(
            envelope.execution_payload.block_number(),
            envelope.execution_payload.timestamp(),
        );
    }
}

impl<EngineClient_, DerivationClient> EngineRequestReceiver
    for EngineProcessor<EngineClient_, DerivationClient>
where
    EngineClient_: EngineClient + 'static,
    DerivationClient: EngineDerivationClient + 'static,
{
    fn start(
        mut self,
        mut request_channel: mpsc::Receiver<EngineProcessingRequest>,
    ) -> JoinHandle<Result<(), EngineError>> {
        tokio::spawn(async move {
            // Bootstrap: pre-populate the unsafe_head_tx watch channel so that external callers
            // (admin_startSequencer, op_syncStatus) never observe a zero hash.
            //
            // We gate on whether reth's current head is at the rollup genesis:
            //
            //   • At genesis — reth has no snap-synced canonical chain, so engine.reset() is
            //     safe: it FCUs to the genesis block and sets up derivation normally. The
            //     el_sync_finished / el_sync_complete gate is preserved as before.
            //
            //   • Beyond genesis — reth already has a canonical chain (e.g. after snap sync).
            //     Sending a FCU to the sync-start block would reorg reth below its state pivot,
            //     causing every subsequent engine_newPayload to return Syncing and the node to
            //     enter an infinite reset loop. Instead we seed the watch channel from reth's
            //     current head directly; derivation will issue its own FCU once the first Reset
            //     task arrives.
            let reth_head = self.client.l2_block_info_by_label(BlockNumberOrTag::Latest).await;
            let at_genesis = match &reth_head {
                Ok(Some(head)) => head.block_info.hash == self.rollup.genesis.l2.hash,
                Ok(None) => true,
                Err(err) => {
                    warn!(target: "engine", ?err, "Bootstrap: failed to query reth head, falling back to reset");
                    true
                }
            };

            if at_genesis {
                match self.engine.reset(Arc::clone(&self.client), Arc::clone(&self.rollup)).await {
                    Ok(_) => {
                        if let Some(unsafe_head_tx) = self.unsafe_head_tx.as_ref() {
                            let new_head = self.engine.state().sync_state.unsafe_head();
                            unsafe_head_tx.send_if_modified(|val| {
                                (*val != new_head).then(|| *val = new_head).is_some()
                            });
                        }
                    }
                    Err(err) => {
                        warn!(target: "engine", ?err, "Engine startup bootstrap failed; will initialize on first task");
                    }
                }
            } else if let Ok(Some(head)) = reth_head {
                //   Beyond genesis — reth already has a canonical chain (e.g. after snap sync).
                //   Query safe and finalized heads optimistically; if unavailable (chain just
                //   started, nothing finalized yet) fall back to default and let derivation fill
                //   them in once the first task drains.
                let safe = self
                    .client
                    .l2_block_info_by_label(BlockNumberOrTag::Safe)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let finalized = self
                    .client
                    .l2_block_info_by_label(BlockNumberOrTag::Finalized)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default();

                // Probe the EL with a FCU pointing to reth's own current canonical heads.
                // This distinguishes two cases:
                //
                //   • Valid   — reth's chain is complete (post snap-sync or normal restart).
                //               el_sync_finished is set to true immediately, so any incoming
                //               Reset request (e.g. from schedule_initial_reset) is not
                //               blocked by the ELSyncing guard.
                //
                //   • Syncing — reth is still snap-syncing. el_sync_finished stays false.
                //               Behaviour is identical to the pre-fix path; the sequencer's
                //               schedule_initial_reset loop keeps retrying until the EL is
                //               ready (e.g. when a P2P unsafe block triggers InsertTask).
                //
                // IMPORTANT: the probe must be called before seed_state. SynchronizeTask
                // short-circuits (skips the FCU) when state.sync_state already equals
                // new_sync_state. Calling seed_state first would cause the probe to silently
                // do nothing, leaving el_sync_finished = false permanently.
                let probe_update = EngineSyncStateUpdate {
                    unsafe_head: Some(head),
                    cross_unsafe_head: Some(head),
                    local_safe_head: Some(safe),
                    safe_head: Some(safe),
                    finalized_head: Some(finalized),
                };
                let el_confirmed = match self
                    .engine
                    .probe_el_sync(Arc::clone(&self.client), Arc::clone(&self.rollup), probe_update)
                    .await
                {
                    Ok(confirmed) => confirmed,
                    Err(err) => {
                        warn!(
                            target: "engine",
                            error = ?err,
                            "Bootstrap: FCU probe failed, treating EL as syncing"
                        );
                        false
                    }
                };

                if !el_confirmed {
                    // Snap-sync still in progress or probe failed. Seed the watch channel
                    // so op_syncStatus never observes zeros during the bootstrap window,
                    // but leave el_sync_finished = false so Reset requests are deferred
                    // until the EL finishes syncing.
                    self.engine.seed_state(probe_update);
                }

                if let Some(unsafe_head_tx) = self.unsafe_head_tx.as_ref() {
                    let new_head = self.engine.state().sync_state.unsafe_head();
                    unsafe_head_tx.send_if_modified(|val| {
                        (*val != new_head).then(|| *val = new_head).is_some()
                    });
                }

                if el_confirmed {
                    info!(
                        target: "engine",
                        unsafe_head = %head.block_info.number,
                        safe_head = %safe.block_info.number,
                        finalized_head = %finalized.block_info.number,
                        "Bootstrap: EL confirmed canonical chain, el_sync_finished = true"
                    );
                } else {
                    info!(
                        target: "engine",
                        unsafe_head = %head.block_info.number,
                        safe_head = %safe.block_info.number,
                        finalized_head = %finalized.block_info.number,
                        "Bootstrap: EL sync pending (snap-sync in progress), seeded engine state from reth"
                    );
                }
            }

            loop {
                // Attempt to drain all outstanding tasks from the engine queue before adding new
                // ones.
                self.drain().await.inspect_err(
                    |err| error!(target: "engine", ?err, "Failed to drain engine tasks"),
                )?;

                // If the unsafe head has updated, propagate it to the outbound channels.
                if let Some(unsafe_head_tx) = self.unsafe_head_tx.as_ref() {
                    unsafe_head_tx.send_if_modified(|val| {
                        let new_head = self.engine.state().sync_state.unsafe_head();
                        (*val != new_head).then(|| *val = new_head).is_some()
                    });
                }

                // Wait for the next processing request.
                let Some(request) = request_channel.recv().await else {
                    error!(target: "engine", "Engine processing request receiver closed unexpectedly");
                    return Err(EngineError::ChannelClosed);
                };

                match request {
                    EngineProcessingRequest::Build(build_request) => {
                        let BuildRequest { attributes, result_tx } = *build_request;
                        let task = EngineTask::Build(Box::new(BuildTask::new(
                            Arc::clone(&self.client),
                            Arc::clone(&self.rollup),
                            attributes,
                            Some(result_tx),
                        )));
                        self.engine.enqueue(task);
                    }
                    EngineProcessingRequest::GetPayload(get_payload_request) => {
                        let GetPayloadRequest { payload_id, attributes, result_tx } =
                            *get_payload_request;
                        let task = EngineTask::GetPayload(Box::new(GetPayloadTask::new(
                            Arc::clone(&self.client),
                            Arc::clone(&self.rollup),
                            payload_id,
                            attributes,
                            Some(result_tx),
                        )));
                        self.engine.enqueue(task);
                    }
                    EngineProcessingRequest::ProcessSafeL2Signal(safe_signal) => {
                        let task = EngineTask::Consolidate(Box::new(ConsolidateTask::new(
                            Arc::clone(&self.client),
                            Arc::clone(&self.rollup),
                            safe_signal,
                        )));
                        self.engine.enqueue(task);
                    }
                    EngineProcessingRequest::ProcessFinalizedL2BlockNumber(
                        finalized_l2_block_number,
                    ) => {
                        // Finalize the L2 block at the provided block number.
                        let task = EngineTask::Finalize(Box::new(FinalizeTask::new(
                            Arc::clone(&self.client),
                            Arc::clone(&self.rollup),
                            *finalized_l2_block_number,
                        )));
                        self.engine.enqueue(task);
                    }
                    EngineProcessingRequest::ProcessUnsafeL2Block(envelope) => {
                        self.log_follower_upgrade_activation(&envelope);
                        let task = EngineTask::Insert(Box::new(InsertTask::new(
                            Arc::clone(&self.client),
                            Arc::clone(&self.rollup),
                            *envelope,
                            false, /* The payload is not derived in this case. This is an unsafe
                                    * block. */
                        )));
                        self.engine.enqueue(task);
                    }
                    EngineProcessingRequest::Reset(reset_request) => {
                        // Do not reset the engine while the EL is still syncing. A Reset sends a
                        // forkchoice_updated to reth pointing at the sync-start block, which will
                        // return Valid and cause reth to set that stale block as canonical,
                        // aborting any in-progress snap sync. Defer until el_sync_finished=true.
                        if !self.engine.state().el_sync_finished {
                            warn!(target: "engine", "Deferring engine reset: EL sync not yet complete");
                            if reset_request
                                .result_tx
                                .send(Err(EngineClientError::ELSyncing))
                                .await
                                .is_err()
                            {
                                warn!(target: "engine", "Sending ELSyncing response failed");
                            }
                            continue;
                        }

                        warn!(target: "engine", "Received reset request");

                        let reset_res = self.reset().await;

                        // Send the result.
                        let response_payload = reset_res
                            .as_ref()
                            .map(|_| ())
                            .map_err(|e| EngineClientError::ResetForkchoiceError(e.to_string()));
                        if reset_request.result_tx.send(response_payload).await.is_err() {
                            warn!(target: "engine", "Sending reset response failed");
                            // If there was an error and we couldn't notify the caller to handle it,
                            // return the error.
                            reset_res?;
                        }
                    }
                    EngineProcessingRequest::Seal(seal_request) => {
                        let SealRequest { payload_id, attributes, result_tx } = *seal_request;
                        let task = EngineTask::Seal(Box::new(SealTask::new(
                            Arc::clone(&self.client),
                            Arc::clone(&self.rollup),
                            payload_id,
                            attributes,
                            // The payload is not derived in this case.
                            false,
                            Some(result_tx),
                        )));
                        self.engine.enqueue(task);
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_eips::BlockNumberOrTag;
    use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
    use base_consensus_engine::{
        Engine, EngineState,
        test_utils::{test_block_info, test_engine_client_builder},
    };
    use base_consensus_genesis::RollupConfig;
    use tokio::sync::{mpsc, watch};

    use crate::{
        EngineClientError, EngineProcessingRequest, EngineProcessor, EngineRequestReceiver,
        ResetRequest, actors::engine::client::MockEngineDerivationClient,
    };

    fn valid_fcu() -> ForkchoiceUpdated {
        ForkchoiceUpdated {
            payload_status: PayloadStatus {
                status: PayloadStatusEnum::Valid,
                latest_valid_hash: None,
            },
            payload_id: None,
        }
    }

    fn syncing_fcu() -> ForkchoiceUpdated {
        ForkchoiceUpdated {
            payload_status: PayloadStatus {
                status: PayloadStatusEnum::Syncing,
                latest_valid_hash: None,
            },
            payload_id: None,
        }
    }

    /// Verifies that when reth is beyond genesis and responds Valid to the bootstrap FCU probe,
    /// `el_sync_finished` is set immediately so that the sequencer's `schedule_initial_reset`
    /// loop is not permanently blocked by the `ELSyncing` guard.
    ///
    /// This is the fix for the leadership-transfer deadlock: previously the "beyond genesis"
    /// bootstrap path only called `seed_state` (no FCU), leaving `el_sync_finished = false`
    /// forever when no P2P unsafe blocks arrived.
    #[tokio::test]
    async fn bootstrap_beyond_genesis_valid_fcu_sets_el_sync_finished() {
        let head = test_block_info(100);
        let safe = test_block_info(90);
        let finalized = test_block_info(80);

        let client = Arc::new(
            test_engine_client_builder()
                .with_block_info_by_tag(BlockNumberOrTag::Latest, head)
                .with_block_info_by_tag(BlockNumberOrTag::Safe, safe)
                .with_block_info_by_tag(BlockNumberOrTag::Finalized, finalized)
                .with_fork_choice_updated_v3_response(valid_fcu())
                .build(),
        );

        let mut mock_derivation = MockEngineDerivationClient::new();
        // Called by send_derivation_actor_safe_head_if_updated in the first drain() loop.
        mock_derivation.expect_send_new_engine_safe_head().returning(|_| Ok(()));
        // Called by mark_el_sync_complete_and_notify_derivation_actor after el_sync_finished
        // becomes true; finalized_head is non-default so reset() is skipped.
        mock_derivation.expect_notify_sync_completed().returning(|_| Ok(()));

        let (state_tx, state_rx) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let engine = Engine::new(EngineState::default(), state_tx, queue_tx);

        let processor = EngineProcessor::new(
            Arc::clone(&client),
            Arc::new(RollupConfig::default()),
            mock_derivation,
            engine,
            None, // validator mode — no unsafe_head_tx needed
        );

        let (req_tx, req_rx) = mpsc::channel(8);
        let handle = processor.start(req_rx);

        // probe_el_sync calls state_sender.send_replace with el_sync_finished=true during
        // the bootstrap, before the main loop starts. wait_for resolves as soon as the watch
        // channel carries a value satisfying the predicate.
        state_rx
            .clone()
            .wait_for(|s| s.el_sync_finished)
            .await
            .expect("state channel closed before el_sync_finished was set");

        // Drop sender to cleanly terminate the spawned task.
        drop(req_tx);
        let result = handle.await.expect("task panicked");
        assert!(
            matches!(result, Err(crate::EngineError::ChannelClosed)),
            "expected ChannelClosed on clean shutdown, got {result:?}"
        );
    }

    /// Verifies that when reth is mid-snap-sync (FCU returns Syncing), `el_sync_finished`
    /// stays false and a subsequent Reset request is correctly deferred with `ELSyncing`.
    ///
    /// This is the pre-existing snap-sync-in-progress path; the fix must not regress it.
    #[tokio::test]
    async fn bootstrap_beyond_genesis_syncing_fcu_defers_reset() {
        let head = test_block_info(100);
        let safe = test_block_info(90);
        let finalized = test_block_info(80);

        let client = Arc::new(
            test_engine_client_builder()
                .with_block_info_by_tag(BlockNumberOrTag::Latest, head)
                .with_block_info_by_tag(BlockNumberOrTag::Safe, safe)
                .with_block_info_by_tag(BlockNumberOrTag::Finalized, finalized)
                .with_fork_choice_updated_v3_response(syncing_fcu())
                .build(),
        );

        let mut mock_derivation = MockEngineDerivationClient::new();
        // Called by send_derivation_actor_safe_head_if_updated after seed_state seeds safe_head.
        mock_derivation.expect_send_new_engine_safe_head().returning(|_| Ok(()));
        // notify_sync_completed must NOT be called: el_sync_finished is still false.

        let (state_tx, state_rx) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let engine = Engine::new(EngineState::default(), state_tx, queue_tx);

        let processor = EngineProcessor::new(
            Arc::clone(&client),
            Arc::new(RollupConfig::default()),
            mock_derivation,
            engine,
            None,
        );

        let (req_tx, req_rx) = mpsc::channel(8);
        let handle = processor.start(req_rx);

        // In the Syncing path, seed_state sets unsafe_head to reth's reported latest block.
        // Wait for that state to be published before sending the Reset.
        state_rx
            .clone()
            .wait_for(|s| s.sync_state.unsafe_head().block_info.number > 0)
            .await
            .expect("state channel closed before seed_state published");

        // Send a Reset — the ELSyncing guard must fire and return ELSyncing.
        let (result_tx, mut result_rx) = mpsc::channel(1);
        req_tx
            .send(EngineProcessingRequest::Reset(Box::new(ResetRequest { result_tx })))
            .await
            .expect("failed to send reset request");

        let response = result_rx.recv().await.expect("response channel closed");
        assert!(
            matches!(response, Err(EngineClientError::ELSyncing)),
            "expected ELSyncing while snap-sync is in progress, got {response:?}"
        );

        drop(req_tx);
        let _ = handle.await;
    }
}
