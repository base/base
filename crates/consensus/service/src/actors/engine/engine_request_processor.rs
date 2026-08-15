use std::{fmt, sync::Arc};

use alloy_eips::BlockNumberOrTag;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_derive::{ResetSignal, Signal};
use base_consensus_engine::{
    ConsolidateTask, Engine, EngineClient, EngineSyncStateUpdate, EngineTask, EngineTaskError,
    EngineTaskErrorSeverity, EngineTaskErrors, FinalizeTask, ForkchoiceCheckpointLabel,
    ForkchoiceCheckpointReader, InsertTask, InsertTaskResult, Metrics as EngineMetrics,
    NoopForkchoiceCheckpointReader, SealTaskError,
};
use base_protocol::L2BlockInfo;
use opentelemetry::context::FutureExt as OtelFutureExt;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

use crate::{
    BuildRequest, CheckpointWriter, Conductor, EngineActorRequest, EngineClientError,
    EngineDerivationClient, EngineError, GetPayloadRequest, InsertUnsafePayloadRequest, NodeMode,
    NoopCheckpointWriter,
};

/// Requires that the implementor handles engine requests via the provided channel.
/// Note: this exists to facilitate unit testing rather than consolidate multiple implementations
/// under a well-thought-out interface.
pub trait EngineRequestReceiver: Send + Sync {
    /// Starts a task to handle engine processing requests.
    fn start(
        self,
        request_channel: mpsc::Receiver<EngineActorRequest>,
    ) -> JoinHandle<Result<(), EngineError>>;
}

/// Classifies the bootstrap behavior for the [`EngineProcessor`].
///
/// Determined once at startup from the node's configuration and (if applicable)
/// a live conductor leadership check.  Each variant maps to a distinct bootstrap
/// path in [`EngineProcessor::start`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapRole {
    /// Pure validator — seed engine state from reth's latest head, no forkchoice update.
    Validator,
    /// Active sequencer — drive forkchoice at genesis or probe the EL with real heads.
    ActiveSequencer,
    /// Conductor follower or stopped sequencer — probe the EL with zeroed safe/finalized heads.
    ConductorFollower,
}

/// Configuration for mode-specific [`EngineProcessor`] behavior.
pub struct EngineProcessorOptions {
    /// The configured node mode.
    pub node_mode: NodeMode,
    /// Channel used to publish unsafe head updates in sequencer mode.
    pub unsafe_head_tx: Option<watch::Sender<L2BlockInfo>>,
    /// Optional conductor client used during sequencer bootstrap.
    pub conductor: Option<Arc<dyn Conductor>>,
    /// Whether the sequencer starts in a stopped state.
    pub sequencer_stopped: bool,
}

impl EngineProcessorOptions {
    /// Maximum allowed forward gap for sequencer external unsafe payloads.
    ///
    /// Larger gaps are treated as deep CL/EL sync and are left to derivation/EL sync rather than
    /// admitting far-future live gossip into reth.
    pub const MAX_SEQUENCER_EXTERNAL_UNSAFE_GAP: u64 = 300;
}

impl fmt::Debug for EngineProcessorOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineProcessorOptions")
            .field("node_mode", &self.node_mode)
            .field("has_unsafe_head_tx", &self.unsafe_head_tx.is_some())
            .field("has_conductor", &self.conductor.is_some())
            .field("sequencer_stopped", &self.sequencer_stopped)
            .finish()
    }
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
    /// Whether the sequencer was started in a stopped state (`--sequencer.stopped`).
    ///
    /// When `true`, the node is configured as a sequencer but should not begin producing
    /// blocks until `admin_startSequencer` is called.  During bootstrap the node behaves
    /// like a [`BootstrapRole::ConductorFollower`] so it does not issue an active-sequencer
    /// forkchoice update before being explicitly started.
    sequencer_stopped: bool,
    /// The configured node mode.
    node_mode: NodeMode,
    /// The last safe head update sent.
    last_safe_head_sent: L2BlockInfo,
    /// The last safe head checkpoint written.
    last_safe_head_checkpointed: L2BlockInfo,
    /// The last finalized head checkpoint written.
    last_finalized_head_checkpointed: L2BlockInfo,
    /// A channel to use to relay the current unsafe head.
    ///
    /// ## Note
    /// This is `Some` when the node is in sequencer mode, and `None` when the node is in validator
    /// mode.
    unsafe_head_tx: Option<watch::Sender<L2BlockInfo>>,

    /// An optional conductor client used to check leadership during bootstrap.
    ///
    /// In a conductor-orchestrated cluster only the **active sequencer** (leader) should probe
    /// the EL with reth's reported safe/finalized heads.  Follower sequencers send a standard
    /// FCU with zeroed safe/finalized so that normal EL sync is not disrupted.
    conductor: Option<Arc<dyn Conductor>>,

    /// The [`RollupConfig`] used to build tasks.
    rollup: Arc<RollupConfig>,
    /// An [`EngineClient`] used for creating engine tasks.
    client: Arc<EngineClient_>,
    /// The [`Engine`] task queue.
    engine: Engine<EngineClient_>,
    /// Reads checkpointed forkchoice state during reset.
    checkpoint_reader: Arc<dyn ForkchoiceCheckpointReader>,
    /// Writes checkpointed forkchoice state after engine state changes.
    checkpoint_writer: Arc<dyn CheckpointWriter>,
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
        options: EngineProcessorOptions,
    ) -> Self {
        Self::new_with_checkpoint(
            client,
            config,
            derivation_client,
            engine,
            options,
            Arc::new(NoopForkchoiceCheckpointReader),
            Arc::new(NoopCheckpointWriter),
        )
    }

    /// Constructs a new [`EngineProcessor`] with checkpoint persistence.
    pub fn new_with_checkpoint(
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        derivation_client: DerivationClient,
        engine: Engine<EngineClient_>,
        options: EngineProcessorOptions,
        checkpoint_reader: Arc<dyn ForkchoiceCheckpointReader>,
        checkpoint_writer: Arc<dyn CheckpointWriter>,
    ) -> Self {
        Self {
            checkpoint_reader,
            checkpoint_writer,
            client,
            conductor: options.conductor,
            derivation_client,
            el_sync_complete: false,
            engine,
            last_finalized_head_checkpointed: L2BlockInfo::default(),
            last_safe_head_checkpointed: L2BlockInfo::default(),
            last_safe_head_sent: L2BlockInfo::default(),
            node_mode: options.node_mode,
            rollup: config,
            sequencer_stopped: options.sequencer_stopped,
            unsafe_head_tx: options.unsafe_head_tx,
        }
    }

    /// Resets the inner [`Engine`] and propagates the reset to the derivation actor.
    async fn reset(&mut self) -> Result<(), EngineError> {
        // Reset the engine, consulting the checkpoint reader if reth has pruned the labeled
        // safe / finalized block bodies (so the L1 info deposit cannot be reconstructed).
        let l2_safe_head = self
            .engine
            .reset_with_checkpoint_reader(
                Arc::clone(&self.client),
                Arc::clone(&self.rollup),
                self.checkpoint_reader.as_ref(),
            )
            .await?;

        self.checkpoint_forkchoice_state_if_updated().await;

        // Signal the derivation actor to reset.
        let signal = ResetSignal { l2_safe_head };
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

    async fn checkpoint_forkchoice_state_if_updated(&mut self) {
        let safe_head = self.engine.state().sync_state.safe_head();
        if safe_head != L2BlockInfo::default() && safe_head != self.last_safe_head_checkpointed {
            match self
                .checkpoint_writer
                .update_checkpoint(ForkchoiceCheckpointLabel::Safe, safe_head)
                .await
            {
                Ok(()) => self.last_safe_head_checkpointed = safe_head,
                Err(err) => warn!(
                    target: "engine",
                    error = %err,
                    block_number = safe_head.block_info.number,
                    block_hash = %safe_head.block_info.hash,
                    "failed to persist safe head checkpoint; continuing without it"
                ),
            }
        }

        let finalized_head = self.engine.state().sync_state.finalized_head();
        if finalized_head != L2BlockInfo::default()
            && finalized_head != self.last_finalized_head_checkpointed
        {
            match self
                .checkpoint_writer
                .update_checkpoint(ForkchoiceCheckpointLabel::Finalized, finalized_head)
                .await
            {
                Ok(()) => self.last_finalized_head_checkpointed = finalized_head,
                Err(err) => warn!(
                    target: "engine",
                    error = %err,
                    block_number = finalized_head.block_info.number,
                    block_hash = %finalized_head.block_info.hash,
                    "failed to persist finalized head checkpoint; continuing without it"
                ),
            }
        }
    }

    /// Handles an [`EngineTaskErrors`] according to its severity.
    async fn handle_engine_task_error(&mut self, err: EngineTaskErrors) -> Result<(), EngineError> {
        let severity = err.severity();
        if severity == EngineTaskErrorSeverity::Critical {
            error!(target: "engine", ?err, "Critical engine task error");
            return Err(err.into());
        }

        self.handle_engine_task_error_severity(severity, format!("{err:?}")).await
    }

    async fn handle_engine_task_error_severity(
        &mut self,
        severity: EngineTaskErrorSeverity,
        error: String,
    ) -> Result<(), EngineError> {
        match severity {
            EngineTaskErrorSeverity::Critical => {
                error!(target: "engine", %error, "Critical engine task error");
                Err(EngineError::CriticalEngineTask(error))
            }
            EngineTaskErrorSeverity::Reset => {
                warn!(target: "engine", %error, "Received reset request");
                self.reset().await
            }
            EngineTaskErrorSeverity::Flush => {
                // This error is encountered when the payload is marked INVALID
                // by the engine api. Post-holocene, the payload is replaced by
                // a "deposits-only" block and re-executed. At the same time,
                // the channel and any remaining buffered batches are flushed.
                warn!(target: "engine", %error, "Invalid payload, Flushing derivation pipeline.");
                match self.derivation_client.send_signal(Signal::FlushChannel).await {
                    Ok(_) => {
                        debug!(target: "engine", "Sent flush signal to derivation actor");
                        Ok(())
                    }
                    Err(err) => {
                        error!(target: "engine", ?err, "Failed to send flush signal to the derivation actor.");
                        Err(EngineError::ChannelClosed)
                    }
                }
            }
            EngineTaskErrorSeverity::Temporary => {
                trace!(target: "engine", %error, "Temporary engine task error");
                Ok(())
            }
        }
    }

    /// Drains the inner [`Engine`] task queue and attempts to update the safe head.
    async fn drain(&mut self) -> Result<(), EngineError> {
        match self.engine.drain().await {
            Ok(_) => {
                trace!(target: "engine", "[ENGINE] tasks drained");
            }
            Err(err) => {
                self.handle_engine_task_error(err).await?;
            }
        }

        self.checkpoint_forkchoice_state_if_updated().await;
        self.send_derivation_actor_safe_head_if_updated().await?;

        if !self.el_sync_complete && self.engine.state().el_sync_finished {
            self.mark_el_sync_complete_and_notify_derivation_actor().await?;
        }

        Ok(())
    }

    fn enqueue_unsafe_payload_insert(
        &mut self,
        envelope: BaseExecutionPayloadEnvelope,
        result_tx: Option<mpsc::Sender<InsertTaskResult>>,
    ) {
        self.log_follower_upgrade_activation(&envelope);
        let task = match result_tx {
            Some(result_tx) => {
                EngineTask::Insert(Box::new(InsertTask::unsafe_payload_with_result(
                    Arc::clone(&self.client),
                    Arc::clone(&self.rollup),
                    envelope,
                    result_tx,
                )))
            }
            None => EngineTask::Insert(Box::new(InsertTask::unsafe_payload(
                Arc::clone(&self.client),
                Arc::clone(&self.rollup),
                envelope,
            ))),
        };
        self.engine.enqueue(task);
    }

    fn handle_external_unsafe_l2_block(&mut self, envelope: BaseExecutionPayloadEnvelope) {
        let block_number = envelope.execution_payload.block_number();
        let sync_state = self.engine.state().sync_state;
        let unsafe_head = sync_state.unsafe_head();

        if self.node_mode.is_validator() {
            info!(
                target: "engine",
                block_number,
                block_hash = %envelope.execution_payload.block_hash(),
                parent_hash = %envelope.execution_payload.parent_hash(),
                "Validator enqueuing external unsafe payload"
            );
            self.enqueue_unsafe_payload_insert(envelope, None);
            return;
        }

        let block_gap = block_number.checked_sub(unsafe_head.block_info.number);
        if block_gap.is_some_and(|gap| {
            gap > 0 && gap <= EngineProcessorOptions::MAX_SEQUENCER_EXTERNAL_UNSAFE_GAP
        }) {
            info!(
                target: "engine",
                block_number,
                block_hash = %envelope.execution_payload.block_hash(),
                parent_hash = %envelope.execution_payload.parent_hash(),
                block_gap = ?block_gap,
                max_external_unsafe_gap = EngineProcessorOptions::MAX_SEQUENCER_EXTERNAL_UNSAFE_GAP,
                "Sequencer enqueuing external unsafe payload within gap limit"
            );
            self.enqueue_unsafe_payload_insert(envelope, None);
            return;
        }

        info!(
            target: "engine",
            block_number,
            block_hash = %envelope.execution_payload.block_hash(),
            parent_hash = %envelope.execution_payload.parent_hash(),
            block_gap = ?block_gap,
            max_external_unsafe_gap = EngineProcessorOptions::MAX_SEQUENCER_EXTERNAL_UNSAFE_GAP,
            unsafe_head_number = unsafe_head.block_info.number,
            unsafe_head_hash = %unsafe_head.block_info.hash,
            "Sequencer dropped external unsafe payload outside gap limit"
        );
    }

    fn handle_local_unsafe_l2_block(
        &mut self,
        envelope: BaseExecutionPayloadEnvelope,
        result_tx: Option<mpsc::Sender<InsertTaskResult>>,
    ) {
        debug!(
            target: "engine",
            block_number = envelope.execution_payload.block_number(),
            block_hash = %envelope.execution_payload.block_hash(),
            parent_hash = %envelope.execution_payload.parent_hash(),
            "Enqueuing local sequencer unsafe payload"
        );
        self.enqueue_unsafe_payload_insert(envelope, result_tx);
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

    fn log_follower_upgrade_activation(&self, envelope: &BaseExecutionPayloadEnvelope) {
        if self.node_mode.is_sequencer() {
            return;
        }

        self.rollup.log_upgrade_activation(
            envelope.execution_payload.block_number(),
            envelope.execution_payload.timestamp(),
            envelope.execution_payload.timestamp().saturating_sub(self.rollup.block_time),
        );
    }

    /// Classifies the bootstrap role from configuration alone (no I/O).
    ///
    /// Decision table:
    ///
    /// | `node_mode` | `sequencer_stopped` | result |
    /// |-------------|---------------------|--------|
    /// | Validator   | any                 | [`BootstrapRole::Validator`] |
    /// | Sequencer   | `true`              | [`BootstrapRole::ConductorFollower`] |
    /// | Sequencer   | `false`             | [`BootstrapRole::ActiveSequencer`]* |
    ///
    /// *Subject to downgrade to [`BootstrapRole::ConductorFollower`] by
    /// [`Self::resolve_bootstrap_role`] if a conductor reports this node is not the leader.
    pub const fn config_bootstrap_role(&self) -> BootstrapRole {
        if self.node_mode.is_validator() {
            BootstrapRole::Validator
        } else if self.sequencer_stopped {
            BootstrapRole::ConductorFollower
        } else {
            BootstrapRole::ActiveSequencer
        }
    }

    /// Resolves the bootstrap role, performing a conductor leadership check when needed.
    ///
    /// Calls [`Self::config_bootstrap_role`] first; only nodes that config-classify as
    /// [`BootstrapRole::ActiveSequencer`] with a conductor configured will make a network
    /// call.  A conductor check failure is treated conservatively as follower.
    pub async fn resolve_bootstrap_role(&self) -> BootstrapRole {
        match self.config_bootstrap_role() {
            role @ (BootstrapRole::Validator | BootstrapRole::ConductorFollower) => role,
            BootstrapRole::ActiveSequencer => match &self.conductor {
                None => BootstrapRole::ActiveSequencer,
                Some(conductor) => match conductor.leader().await {
                    Ok(true) => BootstrapRole::ActiveSequencer,
                    Ok(false) => BootstrapRole::ConductorFollower,
                    Err(err) => {
                        warn!(
                            target: "engine",
                            error = %err,
                            "Bootstrap: conductor leadership check failed, assuming follower"
                        );
                        BootstrapRole::ConductorFollower
                    }
                },
            },
        }
    }

    /// Bootstrap path for pure validators.
    ///
    /// Seeds engine state from reth's current head so sync-status RPC never returns
    /// zeros, but intentionally skips sending a forkchoice update.  `el_sync_finished`
    /// is left `false` and will be set by the first gossip `InsertTask` FCU.
    async fn bootstrap_validator(&mut self, head: Option<L2BlockInfo>) {
        let Some(head) = head else { return };
        let seed = EngineSyncStateUpdate { unsafe_head: Some(head), ..Default::default() };
        self.engine.seed_state(seed);
        info!(
            target: "engine",
            unsafe_head = %head.block_info.number,
            "Bootstrap: validator seeded engine state, awaiting gossip for EL sync"
        );
    }

    /// Bootstrap path for conductor followers and stopped sequencers.
    ///
    /// Probes the EL with reth's current head as unsafe, but zeroed safe/finalized, so
    /// that `el_sync_finished` can be set when reth responds `Valid`.  Unlike pure
    /// validators, conductor followers must have derivation running so they are ready
    /// for leadership transfer; the zeroed safe/finalized avoids disrupting EL sync.
    async fn bootstrap_conductor_follower(&mut self, head: Option<L2BlockInfo>) {
        let Some(head) = head else { return };

        let follower_update =
            EngineSyncStateUpdate { unsafe_head: Some(head), ..Default::default() };

        let el_confirmed = match self
            .engine
            .probe_el_sync(Arc::clone(&self.client), Arc::clone(&self.rollup), follower_update)
            .await
        {
            Ok(c) => c,
            Err(err) => {
                warn!(
                    target: "engine",
                    error = ?err,
                    "Bootstrap: conductor follower probe failed, seeding state"
                );
                false
            }
        };

        if !el_confirmed {
            self.engine.seed_state(follower_update);
        }

        if let Some(unsafe_head_tx) = self.unsafe_head_tx.as_ref() {
            let new_head = self.engine.state().sync_state.unsafe_head();
            unsafe_head_tx
                .send_if_modified(|val| (*val != new_head).then(|| *val = new_head).is_some());
        }

        info!(
            target: "engine",
            el_confirmed,
            unsafe_head = %head.block_info.number,
            "Bootstrap: conductor follower probed EL sync"
        );
    }

    /// Bootstrap path for the active sequencer.
    ///
    /// - At genesis: calls `engine.reset()` to FCU with all heads set to genesis.
    /// - Beyond genesis: probes the EL with reth's own safe/finalized labels so that
    ///   `el_sync_finished` can be set immediately, unblocking the initial derivation reset.
    async fn bootstrap_active_sequencer(&mut self, head: Option<L2BlockInfo>, at_genesis: bool) {
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
        } else if let Some(head) = head {
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

            let probe_update = EngineSyncStateUpdate {
                unsafe_head: Some(head),
                local_safe_head: Some(safe),
                safe_head: Some(safe),
                finalized_head: Some(finalized),
            };

            let el_confirmed = match self
                .engine
                .probe_el_sync(Arc::clone(&self.client), Arc::clone(&self.rollup), probe_update)
                .await
            {
                Ok(c) => c,
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
                self.engine.seed_state(probe_update);
            }

            if let Some(unsafe_head_tx) = self.unsafe_head_tx.as_ref() {
                let new_head = self.engine.state().sync_state.unsafe_head();
                unsafe_head_tx
                    .send_if_modified(|val| (*val != new_head).then(|| *val = new_head).is_some());
            }

            if el_confirmed {
                info!(
                    target: "engine",
                    unsafe_head = %head.block_info.number,
                    "Bootstrap: EL confirmed canonical chain, el_sync_finished = true"
                );
            } else {
                info!(
                    target: "engine",
                    unsafe_head = %head.block_info.number,
                    "Bootstrap: EL sync pending, seeded engine state"
                );
            }
        }
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
        mut request_channel: mpsc::Receiver<EngineActorRequest>,
    ) -> JoinHandle<Result<(), EngineError>> {
        tokio::spawn(async move {
            // Bootstrap: pre-populate the unsafe_head_tx watch channel so that external callers
            // (admin_startSequencer, sync-status RPC) never observe a zero hash.
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

            let role = self.resolve_bootstrap_role().await;
            let opt_head = reth_head.ok().flatten();
            match role {
                BootstrapRole::Validator => self.bootstrap_validator(opt_head).await,
                BootstrapRole::ConductorFollower => {
                    self.bootstrap_conductor_follower(opt_head).await
                }
                BootstrapRole::ActiveSequencer => {
                    self.bootstrap_active_sequencer(opt_head, at_genesis).await
                }
            }

            loop {
                // Full processor iteration window: drain + recv wait + request handling.
                // Bounds the worst-case channel wait — any request arriving during this
                // iteration waits at most this long before the next recv picks it up.
                let _iter_timer =
                    base_metrics::timed!(EngineMetrics::engine_processor_iteration_duration());

                // Attempt to drain all outstanding tasks from the engine queue before adding new
                // ones.
                base_metrics::time!(EngineMetrics::engine_processor_drain_duration_seconds(), {
                    self.drain().await.inspect_err(
                        |err| error!(target: "engine", ?err, "Failed to drain engine tasks"),
                    )
                })?;

                // If the unsafe head has updated, propagate it to the outbound channels.
                if let Some(unsafe_head_tx) = self.unsafe_head_tx.as_ref() {
                    unsafe_head_tx.send_if_modified(|val| {
                        let new_head = self.engine.state().sync_state.unsafe_head();
                        (*val != new_head).then(|| *val = new_head).is_some()
                    });
                }

                // Wait for the next processing request.
                let recv_result = base_metrics::time!(
                    EngineMetrics::engine_processor_recv_wait_duration_seconds(),
                    { request_channel.recv().await }
                );
                let Some(request) = recv_result else {
                    error!(target: "engine", "Engine processing request receiver closed unexpectedly");
                    return Err(EngineError::ChannelClosed);
                };

                match request {
                    EngineActorRequest::BuildRequest(build_request) => {
                        let BuildRequest { attributes, result_tx, otel_cx } = *build_request;
                        let build_result = self
                            .engine
                            .build(Arc::clone(&self.client), Arc::clone(&self.rollup), attributes)
                            .with_context(otel_cx)
                            .await;
                        match build_result {
                            Ok(payload_id) => {
                                result_tx
                                    .send(Ok(payload_id))
                                    .await
                                    .map_err(|_| EngineError::ChannelClosed)?;
                            }
                            Err(err) => {
                                let severity = err.severity();
                                let error = format!("{err:?}");
                                result_tx
                                    .send(Err(err))
                                    .await
                                    .map_err(|_| EngineError::ChannelClosed)?;
                                self.handle_engine_task_error_severity(severity, error).await?;
                            }
                        }
                    }
                    EngineActorRequest::GetPayloadRequest(get_payload_request) => {
                        let GetPayloadRequest { payload_id, attributes, result_tx, otel_cx } =
                            *get_payload_request;
                        let result = self
                            .engine
                            .get_payload(
                                Arc::clone(&self.client),
                                Arc::clone(&self.rollup),
                                payload_id,
                                attributes,
                            )
                            .with_context(otel_cx)
                            .await;

                        let error =
                            result.as_ref().err().map(|err| (err.severity(), format!("{err:?}")));
                        result_tx.send(result).await.map_err(|err| {
                            EngineTaskErrors::Seal(SealTaskError::MpscSend(Box::new(err)))
                        })?;
                        if let Some((severity, error)) = error {
                            self.handle_engine_task_error_severity(severity, error).await?;
                        }
                    }
                    EngineActorRequest::ProcessSafeL2SignalRequest(safe_signal) => {
                        let task = EngineTask::Consolidate(Box::new(ConsolidateTask::new(
                            Arc::clone(&self.client),
                            Arc::clone(&self.rollup),
                            safe_signal,
                        )));
                        self.engine.enqueue(task);
                    }
                    EngineActorRequest::ProcessFinalizedL2BlockNumberRequest(
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
                    EngineActorRequest::ProcessUnsafeL2BlockRequest(envelope) => {
                        self.handle_external_unsafe_l2_block(*envelope);
                    }
                    EngineActorRequest::ProcessLocalUnsafeL2BlockRequest(envelope) => {
                        let InsertUnsafePayloadRequest { envelope, result_tx, otel_cx } = *envelope;
                        // Attach for the synchronous enqueue call only — no await, no Send issue.
                        let _guard = otel_cx.attach();
                        self.handle_local_unsafe_l2_block(envelope, result_tx);
                    }
                    EngineActorRequest::ResetRequest(reset_request) => {
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
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::transaction::Recovered;
    use alloy_eips::{BlockId, BlockNumHash, BlockNumberOrTag, NumHash, eip2718::Encodable2718};
    use alloy_primitives::{Address, B256, Bloom, Sealed, U256};
    use alloy_rpc_types_engine::{
        ExecutionPayloadV1, ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum,
    };
    use alloy_rpc_types_eth::{Block as RpcBlock, BlockTransactions};
    use async_trait::async_trait;
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use base_common_genesis::{ChainGenesis, RollupConfig, SystemConfig};
    use base_common_rpc_types::Transaction as BaseTransaction;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_consensus_derive::Signal;
    use base_consensus_engine::{
        Engine, EngineState, EngineTaskError, EngineTaskErrorSeverity, ForkchoiceCheckpointError,
        ForkchoiceCheckpointLabel, ForkchoiceCheckpointReader,
        test_utils::{
            TestAttributesBuilder, TestEngineStateBuilder, test_block_info,
            test_engine_client_builder,
        },
    };
    use base_protocol::{BlockInfo, L1BlockInfoBedrock, L2BlockInfo};
    use rstest::rstest;
    use tokio::sync::{mpsc, watch};

    use crate::{
        BuildRequest, EngineActorRequest, EngineClientError, EngineProcessor,
        EngineProcessorOptions, EngineRequestReceiver, MockConductor, NodeMode,
        NoopCheckpointWriter, ResetRequest, actors::engine::client::MockEngineDerivationClient,
    };

    /// Test-only [`ForkchoiceCheckpointReader`] that returns pre-seeded safe/finalized heads.
    ///
    /// Used by the validator-restart regression test to simulate the on-disk checkpoint state
    /// that survives a process restart even when reth has pruned the corresponding block body.
    #[derive(Debug)]
    struct TestCheckpointReader {
        safe: Option<L2BlockInfo>,
        finalized: Option<L2BlockInfo>,
    }

    #[async_trait]
    impl ForkchoiceCheckpointReader for TestCheckpointReader {
        async fn checkpoint(
            &self,
            label: ForkchoiceCheckpointLabel,
        ) -> Result<Option<L2BlockInfo>, ForkchoiceCheckpointError> {
            Ok(match label {
                ForkchoiceCheckpointLabel::Safe => self.safe,
                ForkchoiceCheckpointLabel::Finalized => self.finalized,
            })
        }
    }

    /// Returns a default all-zero L2 block and its canonical hash.
    ///
    /// Use the returned hash as `genesis.l2.hash` in the test rollup config so that
    /// [`L2BlockInfo::from_block_and_genesis`] accepts the block via the genesis path.
    fn make_genesis_block() -> (RpcBlock<BaseTransaction>, B256) {
        let block = RpcBlock::<BaseTransaction>::default();
        let hash = block.clone().into_consensus().hash_slow();
        (block, hash)
    }

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

    fn l2_head(number: u64, hash: B256) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo { number, hash, ..Default::default() },
            ..Default::default()
        }
    }

    fn unsafe_payload(
        block_number: u64,
        parent_hash: B256,
        block_hash: B256,
    ) -> BaseExecutionPayloadEnvelope {
        BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: BaseExecutionPayload::V1(ExecutionPayloadV1 {
                parent_hash,
                fee_recipient: Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: block_number,
                extra_data: Default::default(),
                base_fee_per_gas: U256::ZERO,
                block_hash,
                transactions: vec![],
            }),
        }
    }

    fn unsafe_payload_processor(
        node_mode: NodeMode,
        el_sync_finished: bool,
        unsafe_head: L2BlockInfo,
        safe_head: Option<L2BlockInfo>,
    ) -> (
        EngineProcessor<
            base_consensus_engine::test_utils::MockEngineClient,
            MockEngineDerivationClient,
        >,
        watch::Receiver<usize>,
    ) {
        let client = Arc::new(test_engine_client_builder().build());
        let config = Arc::new(RollupConfig::default());
        let derivation_client = MockEngineDerivationClient::new();
        let mut initial_state_builder = TestEngineStateBuilder::new()
            .with_unsafe_head(unsafe_head)
            .with_el_sync_finished(el_sync_finished);
        if let Some(safe_head) = safe_head {
            initial_state_builder = initial_state_builder.with_safe_head(safe_head);
        }
        let initial_state = initial_state_builder.build();
        let (state_tx, _) = watch::channel(initial_state);
        let (queue_tx, queue_rx) = watch::channel(0usize);
        let engine = Engine::new(initial_state, state_tx, queue_tx);
        let unsafe_head_tx = if node_mode.is_sequencer() {
            let (tx, _) = watch::channel(L2BlockInfo::default());
            Some(tx)
        } else {
            None
        };

        (
            EngineProcessor::new(
                client,
                config,
                derivation_client,
                engine,
                EngineProcessorOptions {
                    node_mode,
                    unsafe_head_tx,
                    conductor: None,
                    sequencer_stopped: false,
                },
            ),
            queue_rx,
        )
    }

    #[rstest]
    #[case::sequencer_enqueues_contiguous_external_payload(
        NodeMode::Sequencer,
        true,
        l2_head(10, B256::with_last_byte(10)),
        None,
        false,
        unsafe_payload(11, B256::with_last_byte(10), B256::with_last_byte(11)),
        1
    )]
    #[case::sequencer_enqueues_near_tip_external_payload_when_safe_is_behind(
        NodeMode::Sequencer,
        true,
        l2_head(1_940_222, B256::with_last_byte(22)),
        Some(l2_head(1_940_222, B256::with_last_byte(22))),
        false,
        unsafe_payload(1_940_265, B256::with_last_byte(64), B256::with_last_byte(65)),
        1
    )]
    #[case::sequencer_enqueues_observed_restart_gap_external_payload(
        NodeMode::Sequencer,
        true,
        l2_head(1_939_909, B256::with_last_byte(9)),
        None,
        false,
        unsafe_payload(1_940_000, B256::with_last_byte(99), B256::with_last_byte(100)),
        1
    )]
    #[case::sequencer_enqueues_external_payload_at_gap_boundary(
        NodeMode::Sequencer,
        true,
        l2_head(1_000, B256::with_last_byte(10)),
        None,
        false,
        unsafe_payload(
            1_000 + EngineProcessorOptions::MAX_SEQUENCER_EXTERNAL_UNSAFE_GAP,
            B256::with_last_byte(50),
            B256::with_last_byte(51),
        ),
        1
    )]
    #[case::sequencer_drops_external_payload_beyond_gap_boundary(
        NodeMode::Sequencer,
        true,
        l2_head(1_000, B256::with_last_byte(10)),
        None,
        false,
        unsafe_payload(
            1_000 + EngineProcessorOptions::MAX_SEQUENCER_EXTERNAL_UNSAFE_GAP + 1,
            B256::with_last_byte(50),
            B256::with_last_byte(51),
        ),
        0
    )]
    #[case::sequencer_drops_deep_sync_external_payload(
        NodeMode::Sequencer,
        true,
        l2_head(878_765, B256::with_last_byte(10)),
        None,
        false,
        unsafe_payload(1_936_802, B256::with_last_byte(50), B256::with_last_byte(51)),
        0
    )]
    #[case::sequencer_drops_stale_external_payload(
        NodeMode::Sequencer,
        true,
        l2_head(10, B256::with_last_byte(10)),
        None,
        false,
        unsafe_payload(10, B256::with_last_byte(9), B256::with_last_byte(10)),
        0
    )]
    #[case::sequencer_enqueues_external_next_block_with_parent_mismatch(
        NodeMode::Sequencer,
        true,
        l2_head(10, B256::with_last_byte(10)),
        None,
        false,
        unsafe_payload(11, B256::with_last_byte(99), B256::with_last_byte(11)),
        1
    )]
    #[case::sequencer_cl_sync_preserves_local_unsafe_payload_insertion(
        NodeMode::Sequencer,
        true,
        l2_head(10, B256::with_last_byte(10)),
        Some(l2_head(9, B256::with_last_byte(9))),
        true,
        unsafe_payload(11, B256::with_last_byte(10), B256::with_last_byte(11)),
        1
    )]
    #[case::local_sequencer_inserts_old_unsafe_payload_without_gap_limit(
        NodeMode::Sequencer,
        true,
        l2_head(10_000, B256::with_last_byte(10)),
        None,
        true,
        unsafe_payload(6_400, B256::with_last_byte(99), B256::with_last_byte(100)),
        1
    )]
    #[case::validator_preserves_immediate_unsafe_payload_insertion(
        NodeMode::Validator,
        false,
        l2_head(10, B256::with_last_byte(10)),
        None,
        false,
        unsafe_payload(12, B256::with_last_byte(11), B256::with_last_byte(12)),
        1
    )]
    fn unsafe_payload_processing_updates_queue(
        #[case] node_mode: NodeMode,
        #[case] el_sync_finished: bool,
        #[case] unsafe_head: L2BlockInfo,
        #[case] safe_head: Option<L2BlockInfo>,
        #[case] local_payload: bool,
        #[case] envelope: BaseExecutionPayloadEnvelope,
        #[case] expected_queue_len: usize,
    ) {
        let (mut processor, queue_rx) =
            unsafe_payload_processor(node_mode, el_sync_finished, unsafe_head, safe_head);

        if local_payload {
            processor.handle_local_unsafe_l2_block(envelope, None);
        } else {
            processor.handle_external_unsafe_l2_block(envelope);
        }

        assert_eq!(*queue_rx.borrow(), expected_queue_len);
    }

    /// Verifies that when a standalone sequencer (no conductor) is beyond genesis and reth
    /// responds Valid to the bootstrap FCU probe, `el_sync_finished` is set immediately so
    /// that `schedule_initial_reset` is not permanently blocked by the `ELSyncing` guard.
    ///
    /// The active-sequencer path probes reth with its own safe/finalized heads, so
    /// `el_sync_finished` is set to true without waiting for a P2P unsafe block.
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
        // Called by send_derivation_actor_safe_head_if_updated in the first drain() loop:
        // safe_head is advanced to block_90 so it differs from last_safe_head_sent.
        mock_derivation.expect_send_new_engine_safe_head().returning(|_| Ok(()));
        // Called by mark_el_sync_complete_and_notify_derivation_actor after el_sync_finished
        // becomes true; finalized_head is non-default (block_80) so reset() is skipped.
        mock_derivation.expect_notify_sync_completed().returning(|_| Ok(()));

        let (state_tx, state_rx) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let engine = Engine::new(EngineState::default(), state_tx, queue_tx);

        // Sequencer mode: unsafe_head_tx is Some. No conductor → standalone sequencer → active.
        let (unsafe_head_tx, _) = watch::channel(L2BlockInfo::default());

        let processor = EngineProcessor::new(
            Arc::clone(&client),
            Arc::new(RollupConfig::default()),
            mock_derivation,
            engine,
            EngineProcessorOptions {
                node_mode: NodeMode::Sequencer,
                unsafe_head_tx: Some(unsafe_head_tx),
                conductor: None,
                sequencer_stopped: false,
            },
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
    /// Tests the standalone sequencer path (`unsafe_head_tx` = Some, no conductor).
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
        // In the Syncing path, seed_state advances safe_head (block_90) so
        // send_derivation_actor_safe_head_if_updated fires after seed.
        mock_derivation.expect_send_new_engine_safe_head().returning(|_| Ok(()));
        // notify_sync_completed must NOT be called: el_sync_finished is still false.

        let (state_tx, state_rx) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let engine = Engine::new(EngineState::default(), state_tx, queue_tx);

        // Sequencer mode (unsafe_head_tx = Some). No conductor → standalone sequencer → active.
        let (unsafe_head_tx, _) = watch::channel(L2BlockInfo::default());

        let processor = EngineProcessor::new(
            Arc::clone(&client),
            Arc::new(RollupConfig::default()),
            mock_derivation,
            engine,
            EngineProcessorOptions {
                node_mode: NodeMode::Sequencer,
                unsafe_head_tx: Some(unsafe_head_tx),
                conductor: None,
                sequencer_stopped: false,
            },
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
            .send(EngineActorRequest::ResetRequest(Box::new(ResetRequest { result_tx })))
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

    /// Verifies that a conductor follower sequencer (conductor reports `leader() = false`)
    /// probes reth and sets `el_sync_finished` so it is ready for leadership transfer.
    ///
    /// Unlike pure validators, conductor followers must have derivation running to be
    /// eligible for leadership transfer.  They probe with zeroed safe/finalized (not
    /// reth's labels), and when reth responds `Valid`, `el_sync_finished` is set.
    ///
    /// This test catches a regression where conductor followers were incorrectly treated
    /// as pure validators (seed-only, no probe), leaving `el_sync_finished = false`
    /// permanently and breaking conductor leadership transfer.
    #[tokio::test]
    async fn bootstrap_beyond_genesis_conductor_follower_probes_and_sets_el_sync_finished() {
        let head = test_block_info(100);

        // Conductor follower probes with zeroed safe/finalized — needs a Valid FCU response.
        let client = Arc::new(
            test_engine_client_builder()
                .with_block_info_by_tag(BlockNumberOrTag::Latest, head)
                .with_fork_choice_updated_v3_response(valid_fcu())
                .build(),
        );

        let mut mock_derivation = MockEngineDerivationClient::new();
        // el_sync_finished is set (Valid) → mark_el_sync_complete fires → reset + notify.
        mock_derivation.expect_send_new_engine_safe_head().returning(|_| Ok(()));
        mock_derivation.expect_notify_sync_completed().returning(|_| Ok(()));
        mock_derivation.expect_send_signal().returning(|_| Ok(()));

        let mut mock_conductor = MockConductor::new();
        mock_conductor.expect_leader().returning(|| Ok(false));

        let (state_tx, state_rx) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let engine = Engine::new(EngineState::default(), state_tx, queue_tx);

        let (unsafe_head_tx, _) = watch::channel(L2BlockInfo::default());

        let processor = EngineProcessor::new(
            Arc::clone(&client),
            Arc::new(RollupConfig::default()),
            mock_derivation,
            engine,
            EngineProcessorOptions {
                node_mode: NodeMode::Sequencer,
                unsafe_head_tx: Some(unsafe_head_tx),
                conductor: Some(Arc::new(mock_conductor)),
                sequencer_stopped: false,
            },
        );

        let (req_tx, req_rx) = mpsc::channel(8);
        let handle = processor.start(req_rx);

        // Conductor follower must set el_sync_finished via the probe so it is ready
        // for leadership transfer.
        state_rx
            .clone()
            .wait_for(|s| s.el_sync_finished)
            .await
            .expect("conductor follower must set el_sync_finished from bootstrap probe");

        // Safe/finalized should be zeroed — the probe used zeroed values.
        let state = state_rx.borrow();
        assert_eq!(
            state.sync_state.safe_head(),
            L2BlockInfo::default(),
            "conductor follower should have zeroed safe head"
        );

        drop(req_tx);
        let _ = handle.await;
    }

    /// Regression test: demonstrates that a validator node (`unsafe_head_tx` = None) was
    /// incorrectly using reth's reported safe/finalized heads in the bootstrap FCU instead
    /// of sending zeroed values.
    ///
    /// On unfixed main the beyond-genesis path queries reth's Safe/Finalized tags
    /// unconditionally and builds a `probe_update` with those non-zero values.  After a Valid
    /// FCU response the engine sync state is seeded with those values, so `safe_head` becomes
    /// block 50 rather than staying zeroed.
    ///
    /// After the fix, validators take the follower path and send a FCU with only the unsafe
    /// head, leaving safe/finalized zeroed and not disrupting EL snap-sync.
    ///
    /// This test FAILS on unfixed main and PASSES after the fix lands.
    #[tokio::test]
    async fn bootstrap_beyond_genesis_validator_sends_zeroed_safe_finalized() {
        let head = test_block_info(100);
        // Non-zero safe/finalized — this is what reth reports and what the unfixed path uses.
        let reth_safe = test_block_info(50);
        let reth_finalized = test_block_info(40);

        let client = Arc::new(
            test_engine_client_builder()
                .with_block_info_by_tag(BlockNumberOrTag::Latest, head)
                .with_block_info_by_tag(BlockNumberOrTag::Safe, reth_safe)
                .with_block_info_by_tag(BlockNumberOrTag::Finalized, reth_finalized)
                .with_fork_choice_updated_v3_response(valid_fcu())
                .build(),
        );

        // No derivation calls: el_sync_finished stays false on the fixed validator path so
        // mark_el_sync_complete_and_notify_derivation_actor never fires.
        let mock_derivation = MockEngineDerivationClient::new();

        let (state_tx, state_rx) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let engine = Engine::new(EngineState::default(), state_tx, queue_tx);

        // Validator mode: unsafe_head_tx = None.
        let processor = EngineProcessor::new(
            Arc::clone(&client),
            Arc::new(RollupConfig::default()),
            mock_derivation,
            engine,
            EngineProcessorOptions {
                node_mode: NodeMode::Validator,
                unsafe_head_tx: None,
                conductor: None,
                sequencer_stopped: false,
            },
        );

        let (req_tx, req_rx) = mpsc::channel(8);
        let handle = processor.start(req_rx);

        // Close the channel so the task exits after bootstrap + one drain.
        drop(req_tx);
        let _ = handle.await;

        // After the fix: validators take the seed-only path; el_sync_finished stays false
        // and safe/finalized heads are never populated from reth's reported values.
        let state = state_rx.borrow();
        assert!(
            !state.el_sync_finished,
            "validator must not set el_sync_finished during bootstrap"
        );
        assert_eq!(
            state.sync_state.safe_head(),
            L2BlockInfo::default(),
            "validator must not set safe head to reth's reported safe head (expected zeroed, got block {})",
            state.sync_state.safe_head().block_info.number,
        );
        assert_eq!(
            state.sync_state.finalized_head(),
            L2BlockInfo::default(),
            "validator must not set finalized head to reth's reported finalized head (expected zeroed, got block {})",
            state.sync_state.finalized_head().block_info.number,
        );
    }

    /// Verifies that a validator node (`unsafe_head_tx` = None, no conductor) seeds engine
    /// state without sending a bootstrap FCU or setting `el_sync_finished`.
    ///
    /// The validator path must not probe reth — doing so would trivially return Valid
    /// (reth has its own head from the snapshot), prematurely setting `el_sync_finished`
    /// and triggering the engine reset that sends non-zero safe/finalized.  Instead,
    /// `el_sync_finished` is left false and will be set by the first gossip `InsertTask`
    /// FCU.
    #[tokio::test]
    async fn bootstrap_beyond_genesis_validator_seeds_without_probing_el_sync() {
        let head = test_block_info(100);

        // No FCU response configured — no FCU should be sent during bootstrap.
        let client = Arc::new(
            test_engine_client_builder()
                .with_block_info_by_tag(BlockNumberOrTag::Latest, head)
                .build(),
        );

        // No derivation calls: el_sync_finished stays false so
        // mark_el_sync_complete_and_notify_derivation_actor never fires.
        let mock_derivation = MockEngineDerivationClient::new();

        let (state_tx, state_rx) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let engine = Engine::new(EngineState::default(), state_tx, queue_tx);

        let processor = EngineProcessor::new(
            Arc::clone(&client),
            Arc::new(RollupConfig::default()),
            mock_derivation,
            engine,
            EngineProcessorOptions {
                node_mode: NodeMode::Validator,
                unsafe_head_tx: None,
                conductor: None,
                sequencer_stopped: false,
            },
        );

        let (req_tx, req_rx) = mpsc::channel(8);
        let handle = processor.start(req_rx);

        // Close the channel so the task exits after bootstrap + one drain.
        drop(req_tx);
        let _ = handle.await;

        // el_sync_finished must remain false — only a gossip InsertTask FCU may set it.
        let state = state_rx.borrow();
        assert!(
            !state.el_sync_finished,
            "validator must not set el_sync_finished during bootstrap"
        );
        assert_eq!(
            state.sync_state.unsafe_head().block_info.number,
            100,
            "unsafe head should be seeded from reth's latest"
        );
        assert_eq!(
            state.sync_state.safe_head(),
            L2BlockInfo::default(),
            "safe head must remain zeroed"
        );
        assert_eq!(
            state.sync_state.finalized_head(),
            L2BlockInfo::default(),
            "finalized head must remain zeroed"
        );
    }

    // ── config_bootstrap_role / resolve_bootstrap_role unit tests ─────────────────────────

    /// Builds a minimal `EngineProcessor` for testing `config_bootstrap_role` and
    /// `resolve_bootstrap_role` without spinning up a live engine or derivation actor.
    fn test_processor(
        is_sequencer: bool,
        sequencer_stopped: bool,
        conductor: Option<Arc<dyn crate::Conductor>>,
    ) -> EngineProcessor<
        base_consensus_engine::test_utils::MockEngineClient,
        MockEngineDerivationClient,
    > {
        let client = Arc::new(test_engine_client_builder().build());
        let config = Arc::new(RollupConfig::default());
        let derivation_client = MockEngineDerivationClient::new();
        let (state_tx, _) = watch::channel(base_consensus_engine::EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let engine = Engine::new(base_consensus_engine::EngineState::default(), state_tx, queue_tx);
        let unsafe_head_tx = if is_sequencer {
            let (tx, _) = watch::channel(L2BlockInfo::default());
            Some(tx)
        } else {
            None
        };
        EngineProcessor::new(
            client,
            config,
            derivation_client,
            engine,
            EngineProcessorOptions {
                node_mode: if is_sequencer { NodeMode::Sequencer } else { NodeMode::Validator },
                unsafe_head_tx,
                conductor,
                sequencer_stopped,
            },
        )
    }

    #[test]
    fn config_bootstrap_role_validator() {
        let p = test_processor(false, false, None);
        assert_eq!(p.config_bootstrap_role(), super::BootstrapRole::Validator);
    }

    #[test]
    fn config_bootstrap_role_stopped_sequencer_is_follower() {
        let p = test_processor(true, true, None);
        assert_eq!(p.config_bootstrap_role(), super::BootstrapRole::ConductorFollower);
    }

    #[test]
    fn config_bootstrap_role_active_sequencer() {
        let p = test_processor(true, false, None);
        assert_eq!(p.config_bootstrap_role(), super::BootstrapRole::ActiveSequencer);
    }

    #[tokio::test]
    async fn resolve_bootstrap_role_validator_skips_conductor() {
        // Even with a conductor present, a validator must stay Validator without calling leader().
        let mut mock_conductor = MockConductor::new();
        mock_conductor.expect_leader().never();
        let p = test_processor(false, false, Some(Arc::new(mock_conductor)));
        assert_eq!(p.resolve_bootstrap_role().await, super::BootstrapRole::Validator);
    }

    #[tokio::test]
    async fn resolve_bootstrap_role_stopped_sequencer_skips_conductor() {
        // A stopped sequencer must stay ConductorFollower without calling leader().
        let mut mock_conductor = MockConductor::new();
        mock_conductor.expect_leader().never();
        let p = test_processor(true, true, Some(Arc::new(mock_conductor)));
        assert_eq!(p.resolve_bootstrap_role().await, super::BootstrapRole::ConductorFollower);
    }

    #[tokio::test]
    async fn resolve_bootstrap_role_no_conductor_is_active() {
        let p = test_processor(true, false, None);
        assert_eq!(p.resolve_bootstrap_role().await, super::BootstrapRole::ActiveSequencer);
    }

    #[tokio::test]
    async fn resolve_bootstrap_role_conductor_leader_true() {
        let mut mock_conductor = MockConductor::new();
        mock_conductor.expect_leader().once().returning(|| Ok(true));
        let p = test_processor(true, false, Some(Arc::new(mock_conductor)));
        assert_eq!(p.resolve_bootstrap_role().await, super::BootstrapRole::ActiveSequencer);
    }

    #[tokio::test]
    async fn resolve_bootstrap_role_conductor_leader_false() {
        let mut mock_conductor = MockConductor::new();
        mock_conductor.expect_leader().once().returning(|| Ok(false));
        let p = test_processor(true, false, Some(Arc::new(mock_conductor)));
        assert_eq!(p.resolve_bootstrap_role().await, super::BootstrapRole::ConductorFollower);
    }

    #[tokio::test]
    async fn resolve_bootstrap_role_conductor_error_is_follower() {
        use jsonrpsee::core::ClientError;
        let mut mock_conductor = MockConductor::new();
        mock_conductor
            .expect_leader()
            .once()
            .returning(|| Err(crate::ConductorError::Rpc(ClientError::Custom("timeout".into()))));
        let p = test_processor(true, false, Some(Arc::new(mock_conductor)));
        assert_eq!(p.resolve_bootstrap_role().await, super::BootstrapRole::ConductorFollower);
    }

    // ── existing bootstrap integration tests ────────────────────────────────────────────

    /// Regression test: demonstrates that a validator node at genesis was incorrectly calling
    /// `engine.reset()`, which sends a FCU to the EL and — when reth responds Valid — sets
    /// `el_sync_finished = true`.  Reth always responds Valid to a genesis FCU because it always
    /// holds the genesis block, so this prematurely signalled EL sync completion for validators
    /// joining an established network that still need to snap-sync.
    ///
    /// After the fix, validators at genesis call `seed_state()` only; no FCU is sent and
    /// `el_sync_finished` stays false.
    ///
    /// This test FAILS on unfixed main (`el_sync_finished` = true) and PASSES after the fix.
    #[tokio::test]
    async fn bootstrap_at_genesis_validator_seeds_without_probing_el_sync() {
        let (genesis_block, genesis_hash) = make_genesis_block();

        // Build a RollupConfig whose genesis.l2.hash matches the computed hash so that
        // L2BlockInfo::from_block_and_genesis accepts the block via the genesis fast path.
        let cfg = Arc::new(RollupConfig {
            genesis: ChainGenesis {
                l2: BlockNumHash { number: 0, hash: genesis_hash },
                l1: BlockNumHash { number: 0, hash: B256::ZERO },
                system_config: Some(SystemConfig::default()),
                ..Default::default()
            },
            ..Default::default()
        });

        let genesis_l2_info = L2BlockInfo {
            block_info: BlockInfo {
                hash: genesis_hash,
                number: 0,
                parent_hash: B256::ZERO,
                timestamp: 0,
            },
            l1_origin: NumHash { number: 0, hash: B256::ZERO },
            seq_num: 0,
        };

        // On unfixed main, engine.reset() queries: Finalized L2 block, Latest L2 block,
        // the L1 origin of the unsafe head (hash B256::ZERO), FCU v3, then L1 block 0
        // and the L2 safe block by hash for system-config extraction.
        let client = Arc::new(
            test_engine_client_builder()
                .with_config(Arc::clone(&cfg))
                // Bootstrap at_genesis check (l2_block_info_by_label path).
                .with_block_info_by_tag(BlockNumberOrTag::Latest, genesis_l2_info)
                // L2ForkchoiceState::current: Finalized and Latest L2 blocks (get_l2_block path).
                .with_l2_block(BlockId::Number(BlockNumberOrTag::Finalized), genesis_block.clone())
                .with_l2_block(BlockId::Number(BlockNumberOrTag::Latest), genesis_block.clone())
                // find_starting_forkchoice unsafe-head loop: L1 origin of genesis is B256::ZERO.
                .with_l1_block(BlockId::from(B256::ZERO), RpcBlock::default())
                // SynchronizeTask inside engine.reset() sends FCU v3.
                .with_fork_choice_updated_v3_response(valid_fcu())
                // Post-FCU: L1 origin block at number 0 and L2 safe block by genesis hash.
                .with_l1_block(BlockId::from(0u64), RpcBlock::default())
                .with_l2_block(BlockId::from(genesis_hash), genesis_block.clone())
                .build(),
        );

        let mut mock_derivation = MockEngineDerivationClient::new();
        // On unfixed main: engine.reset() succeeds and el_sync_finished is set to true.
        // Then mark_el_sync_complete fires: finalized = genesis (not default) → skip
        // inner reset, call notify_sync_completed. safe_head changes → send_new_engine_safe_head.
        mock_derivation.expect_notify_sync_completed().returning(|_| Ok(()));
        mock_derivation.expect_send_new_engine_safe_head().returning(|_| Ok(()));

        let (state_tx, state_rx) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let engine = Engine::new(EngineState::default(), state_tx, queue_tx);

        // Validator mode: unsafe_head_tx = None.
        let processor = EngineProcessor::new(
            Arc::clone(&client),
            Arc::clone(&cfg),
            mock_derivation,
            engine,
            EngineProcessorOptions {
                node_mode: NodeMode::Validator,
                unsafe_head_tx: None,
                conductor: None,
                sequencer_stopped: false,
            },
        );

        let (req_tx, req_rx) = mpsc::channel(8);
        let handle = processor.start(req_rx);

        drop(req_tx);
        let _ = handle.await;

        // After the fix: validators at genesis only seed internal state without sending a FCU,
        // so el_sync_finished stays false and safe/finalized heads stay zeroed.
        //
        // Before the fix: engine.reset() succeeds, sends a genesis FCU, reth responds Valid
        // (it always holds genesis), setting el_sync_finished = true and stamping safe_head /
        // finalized_head with the genesis L2BlockInfo (hash = genesis_hash, not B256::ZERO).
        let state = state_rx.borrow();
        assert!(
            !state.el_sync_finished,
            "validator at genesis must not set el_sync_finished during bootstrap"
        );
        assert_eq!(
            state.sync_state.safe_head(),
            L2BlockInfo::default(),
            "validator at genesis must not set safe_head via engine.reset() (expected zeroed, got hash {})",
            state.sync_state.safe_head().block_info.hash,
        );
        assert_eq!(
            state.sync_state.finalized_head(),
            L2BlockInfo::default(),
            "validator at genesis must not set finalized_head via engine.reset() (expected zeroed, got hash {})",
            state.sync_state.finalized_head().block_info.hash,
        );
    }

    fn l1_info_deposit_tx_bytes() -> Vec<u8> {
        BaseTxEnvelope::from(TxDeposit {
            input: L1BlockInfoBedrock::default().encode_calldata(),
            ..Default::default()
        })
        .encoded_2718()
    }

    fn unsafe_payload_with_l1_info(
        block_number: u64,
        parent_hash: B256,
        block_hash: B256,
    ) -> BaseExecutionPayloadEnvelope {
        BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: BaseExecutionPayload::V1(ExecutionPayloadV1 {
                parent_hash,
                fee_recipient: Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: block_number,
                extra_data: Default::default(),
                base_fee_per_gas: U256::ZERO,
                block_hash,
                transactions: vec![l1_info_deposit_tx_bytes().into()],
            }),
        }
    }

    fn pruned_reth_l2_block(number: u64, parent_hash: B256) -> RpcBlock<BaseTransaction> {
        let mut block = RpcBlock::<BaseTransaction>::default();
        block.header.inner.number = number;
        block.header.inner.parent_hash = parent_hash;
        block.header.inner.timestamp = number;
        block.transactions = BlockTransactions::Full(vec![]);
        block
    }

    fn l1_info_rpc_transaction(block_number: u64) -> BaseTransaction {
        let envelope = BaseTxEnvelope::Deposit(Sealed::new_unchecked(
            TxDeposit {
                input: L1BlockInfoBedrock::default().encode_calldata(),
                ..Default::default()
            },
            B256::ZERO,
        ));
        BaseTransaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(envelope, Address::ZERO),
                block_hash: None,
                block_number: Some(block_number),
                block_timestamp: None,
                effective_gas_price: Some(0),
                transaction_index: Some(0),
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
        }
    }

    fn full_reth_l2_block_with_l1_info(
        number: u64,
        parent_hash: B256,
    ) -> RpcBlock<BaseTransaction> {
        let mut block = RpcBlock::<BaseTransaction>::default();
        block.header.inner.number = number;
        block.header.inner.parent_hash = parent_hash;
        block.header.inner.timestamp = number;
        block.transactions = BlockTransactions::Full(vec![l1_info_rpc_transaction(number)]);
        block
    }

    /// Regression test for the validator-restart crash when reth has pruned the body of
    /// the last safe block.
    ///
    /// Before the fix, `EngineProcessor::new` would seed safe/finalized heads from
    /// `L2ForkchoiceState::current(client)`, which calls
    /// `client.l2_block_info_by_label(BlockNumberOrTag::Safe)` and `Finalized`. If reth had
    /// pruned the safe block's body, that call returned `None` and the processor lost the
    /// checkpoint, producing zeroed safe/finalized heads. Any subsequent unsafe payload
    /// insertion then panicked because the engine's invariant "`safe_head.number` <=
    /// `unsafe_head.number`" was satisfied trivially but `attributes.parent` mismatches led
    /// to a `CriticalEngineTask` and the processor crashed during `drain()`.
    ///
    /// After the fix, `new_with_checkpoint` consults the persisted checkpoint reader, which
    /// returns the previously checkpointed safe head even when reth has pruned the block
    /// body. The validator can then accept the next unsafe payload and drain cleanly.
    #[tokio::test]
    async fn validator_restart_does_not_crash_when_reth_safe_block_body_is_pruned() {
        let (genesis_block, genesis_hash) = make_genesis_block();
        let cfg = Arc::new(RollupConfig {
            genesis: ChainGenesis {
                l2: BlockNumHash { number: 0, hash: genesis_hash },
                l1: BlockNumHash { number: 0, hash: B256::ZERO },
                system_config: Some(SystemConfig::default()),
                ..Default::default()
            },
            ..Default::default()
        });

        let parent_hash = B256::with_last_byte(0x41);
        let pruned_safe = pruned_reth_l2_block(44_343_433, parent_hash);
        let safe_hash = pruned_safe.header.inner.hash_slow();
        let safe_checkpoint = L2BlockInfo {
            block_info: BlockInfo {
                number: 44_343_433,
                hash: safe_hash,
                parent_hash,
                timestamp: 44_343_433,
            },
            ..Default::default()
        };
        let reth_latest = safe_checkpoint;
        let next_hash = B256::with_last_byte(0x43);
        let full_latest = full_reth_l2_block_with_l1_info(
            reth_latest.block_info.number + 1,
            reth_latest.block_info.hash,
        );

        let client = Arc::new(
            test_engine_client_builder()
                .with_config(Arc::clone(&cfg))
                .with_l2_block(BlockId::Number(BlockNumberOrTag::Finalized), genesis_block)
                .with_l2_block(BlockId::Number(BlockNumberOrTag::Safe), pruned_safe)
                .with_l2_block(BlockId::Number(BlockNumberOrTag::Latest), full_latest)
                .with_l1_block(BlockId::from(B256::ZERO), RpcBlock::default())
                .with_new_payload_v2_response(PayloadStatus {
                    status: PayloadStatusEnum::Valid,
                    latest_valid_hash: Some(next_hash),
                })
                .with_fork_choice_updated_v3_response(valid_fcu())
                .build(),
        );

        let mut mock_derivation = MockEngineDerivationClient::new();
        mock_derivation.expect_send_signal().returning(|_| Ok(()));
        mock_derivation.expect_send_new_engine_safe_head().returning(|_| Ok(()));
        mock_derivation.expect_notify_sync_completed().returning(|_| Ok(()));

        let (state_tx, _state_rx) = watch::channel(EngineState::default());
        let (queue_tx, _queue_rx) = watch::channel(0usize);
        let engine = Engine::new(EngineState::default(), state_tx, queue_tx);

        let mut processor = EngineProcessor::new_with_checkpoint(
            Arc::clone(&client),
            Arc::clone(&cfg),
            mock_derivation,
            engine,
            EngineProcessorOptions {
                node_mode: NodeMode::Validator,
                unsafe_head_tx: None,
                conductor: None,
                sequencer_stopped: false,
            },
            Arc::new(TestCheckpointReader { safe: Some(safe_checkpoint), finalized: None }),
            Arc::new(NoopCheckpointWriter),
        );

        processor.bootstrap_validator(Some(reth_latest)).await;
        processor.handle_external_unsafe_l2_block(unsafe_payload_with_l1_info(
            reth_latest.block_info.number + 1,
            reth_latest.block_info.hash,
            next_hash,
        ));

        processor
            .drain()
            .await
            .expect("validator restart must not crash when reth pruned historical block bodies");
    }

    /// Regression test: when a `Build` request fails with an `InvalidPayload` (the EL rejects
    /// the derived attributes), the processor must dispatch exactly one
    /// [`Signal::FlushChannel`] to the derivation actor and resume servicing requests rather
    /// than retrying the poisoned task in place. Without the
    /// [`EngineTaskErrorSeverity::Flush`] mapping plus the head-pop in
    /// [`base_consensus_engine::Engine::drain`], the processor would either spin on the same
    /// FCU forever or starve every later request behind the poisoned head.
    #[tokio::test]
    async fn build_invalid_payload_dispatches_flush_signal_exactly_once() {
        let parent_block = test_block_info(0);
        let unsafe_block = test_block_info(1);
        let attributes_timestamp = unsafe_block.block_info.timestamp;

        let mut cfg = RollupConfig::default();
        cfg.upgrades.ecotone_time = Some(attributes_timestamp);
        let cfg = Arc::new(cfg);

        let invalid_fcu = ForkchoiceUpdated {
            payload_status: PayloadStatus {
                status: PayloadStatusEnum::Invalid {
                    validation_error: "malformed transaction".into(),
                },
                latest_valid_hash: Some(B256::with_last_byte(2)),
            },
            payload_id: None,
        };
        let client = Arc::new(
            test_engine_client_builder().with_fork_choice_updated_v3_response(invalid_fcu).build(),
        );

        let (signal_tx, mut signal_rx) = mpsc::channel(4);
        let mut mock_derivation = MockEngineDerivationClient::new();
        // Bootstrap and per-block plumbing calls — accept any number of calls so the
        // test focuses on the Flush dispatch alone.
        mock_derivation.expect_send_new_engine_safe_head().returning(|_| Ok(()));
        mock_derivation.expect_notify_sync_completed().returning(|_| Ok(()));
        mock_derivation
            .expect_send_signal()
            .withf(|s| matches!(s, Signal::FlushChannel))
            .times(1)
            .returning(move |signal| {
                let signal_tx = signal_tx.clone();
                tokio::spawn(async move {
                    let _ = signal_tx.send(signal).await;
                });
                Ok(())
            });

        let initial_state = TestEngineStateBuilder::new()
            .with_unsafe_head(unsafe_block)
            .with_safe_head(parent_block)
            .with_el_sync_finished(true)
            .build();
        let (state_tx, _state_rx) = watch::channel(initial_state);
        let (queue_tx, queue_rx) = watch::channel(0usize);
        let engine = Engine::new(initial_state, state_tx, queue_tx);

        let processor = EngineProcessor::new(
            Arc::clone(&client),
            Arc::clone(&cfg),
            mock_derivation,
            engine,
            EngineProcessorOptions {
                node_mode: NodeMode::Validator,
                unsafe_head_tx: None,
                conductor: None,
                sequencer_stopped: false,
            },
        );

        let (req_tx, req_rx) = mpsc::channel(8);
        let handle = processor.start(req_rx);

        let attributes = TestAttributesBuilder::new()
            .with_parent(parent_block)
            .with_timestamp(attributes_timestamp)
            .build();
        let (build_result_tx, mut build_result_rx) = mpsc::channel(1);
        req_tx
            .send(EngineActorRequest::BuildRequest(Box::new(BuildRequest {
                attributes,
                result_tx: build_result_tx,
                otel_cx: opentelemetry::Context::new(),
            })))
            .await
            .expect("failed to send build request");

        let build_result =
            tokio::time::timeout(std::time::Duration::from_secs(5), build_result_rx.recv())
                .await
                .expect("timed out waiting for build result")
                .expect("build result channel closed before response");
        assert!(matches!(
            build_result,
            Err(err) if err.severity() == EngineTaskErrorSeverity::Flush
        ));

        let received = tokio::time::timeout(std::time::Duration::from_secs(5), signal_rx.recv())
            .await
            .expect("timed out waiting for FlushChannel signal")
            .expect("signal channel closed before FlushChannel was sent");
        assert!(matches!(received, Signal::FlushChannel));

        // Queue must drain back to zero — proves the poisoned task was popped, not re-queued.
        let mut queue_rx = queue_rx;
        tokio::time::timeout(std::time::Duration::from_secs(5), queue_rx.wait_for(|n| *n == 0))
            .await
            .expect("queue length never returned to zero")
            .expect("queue length watch closed before draining");

        // Clean shutdown: dropping the request channel makes start() exit with ChannelClosed.
        // The mockall `times(1)` expectation is verified on drop of `mock_derivation` inside
        // the spawned task — any second call to send_signal would panic the task.
        drop(req_tx);
        let result = handle.await.expect("processor task panicked");
        assert!(
            matches!(result, Err(crate::EngineError::ChannelClosed)),
            "expected ChannelClosed on clean shutdown, got {result:?}"
        );
    }
}
