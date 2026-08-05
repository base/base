use core::time::Duration;
use std::{
    ops::{Div, Rem},
    sync::Arc,
    time::Instant,
};

use alloy_consensus::Transaction;
use alloy_evm::Database;
use alloy_primitives::{Address, B256, U256, map::foldhash::HashMap};
use base_builder_publish::WebSocketPublisher;
use base_bundles::RejectedTransaction;
use base_common_chains::Upgrades;
use base_common_consensus::BaseTransactionSigned;
use base_common_flashblocks::FlashblockId;
use base_execution_evm::{BaseEvmConfig, BaseNextBlockEnvAttributes};
use base_execution_payload_builder::{BaseBuiltPayload, BasePayloadBuilderAttributes};
use base_observability_events::{GlobalTransactionEventWriter, TransactionEventType};
use eyre::WrapErr as _;
use parking_lot::RwLock;
use reth_basic_payload_builder::BuildOutcome;
use reth_evm::{ConfigureEvm, execute::BlockBuilder};
use reth_execution_cache::{CachedStateMetrics, CachedStateMetricsSource, CachedStateProvider};
use reth_execution_types::ChangedAccount;
use reth_node_api::PayloadBuilderError;
use reth_payload_primitives::PayloadAttributes;
use reth_payload_util::BestPayloadTransactions;
use reth_provider::{
    HashedPostStateProvider, ProviderError, StateRootProvider, StorageRootProvider,
};
use reth_revm::{State, database::StateProviderDatabase};
use reth_transaction_pool::TransactionPool;
use revm::Database as _;
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, metadata::Level, span, warn};

use crate::{
    BuilderConfig, BuilderMetrics, CandidateSource, DefaultCandidateSource, ExecutionInfo,
    PayloadBuilder, ResourceLimits,
    flashblocks::{
        FlashblockAssembler, FlashblockBaseMode, FlashblocksExtraCtx, StateRootMode,
        best_txs::BestFlashblocksTxs, context::BasePayloadBuilderCtx, generator::BuildArguments,
    },
    traits::{ClientBounds, PoolBounds},
    transaction_events::{
        BuilderFlashblockPublishedEventData, BuilderFlashblockStartedEventData,
        BuilderFlashblockStoppedEventData, BuilderIncludedEventData,
        BuilderPayloadFinalizedEventData, BuilderTransactionEventContext,
        emit_builder_payload_event, emit_builder_transaction_event,
    },
};

type NextBestFlashblocksTxs<Pool> = BestFlashblocksTxs<
    <Pool as TransactionPool>::Transaction,
    Box<
        dyn reth_transaction_pool::BestTransactions<
                Item = Arc<
                    reth_transaction_pool::ValidPoolTransaction<
                        <Pool as TransactionPool>::Transaction,
                    >,
                >,
            >,
    >,
>;

/// The outbound channels the flashblocks builder emits to.
///
/// Grouped so [`BasePayloadBuilder::new`] takes a single cohesive argument rather than threading
/// each sink through individually.
#[derive(Debug, Clone)]
pub(super) struct BuilderOutputs {
    /// Sender for sending built payloads to [`PayloadHandler`],
    /// which broadcasts outgoing payloads via p2p.
    pub payload_tx: mpsc::Sender<BaseBuiltPayload>,
    /// WebSocket publisher for broadcasting flashblocks
    /// to all connected subscribers.
    pub ws_pub: Arc<WebSocketPublisher>,
    /// Sender for forwarding per-block batches of rejected transactions to the audit-archiver.
    pub rejected_tx_sender: Option<mpsc::Sender<Vec<RejectedTransaction>>>,
}

/// Base payload builder
#[derive(Debug, Clone)]
pub(super) struct BasePayloadBuilder<Pool, Client, S = DefaultCandidateSource> {
    /// The type responsible for creating the evm.
    pub evm_config: BaseEvmConfig,
    /// The transaction pool
    pub pool: Pool,
    /// Node client
    pub client: Client,
    /// System configuration for the builder
    pub config: BuilderConfig,
    /// The outbound channels the builder emits built payloads, flashblocks, and rejected
    /// transactions to.
    pub outputs: BuilderOutputs,
    /// Last flashblock emitted by this builder instance.
    last_emitted_flashblock_id: Arc<RwLock<FlashblockId>>,
    /// Transforms the candidate transaction stream drained by the build loop.
    candidate_source: S,
}

impl<Pool, Client, S> BasePayloadBuilder<Pool, Client, S> {
    /// `BasePayloadBuilder` constructor.
    pub(super) fn new(
        evm_config: BaseEvmConfig,
        pool: Pool,
        client: Client,
        config: BuilderConfig,
        outputs: BuilderOutputs,
        candidate_source: S,
    ) -> Self {
        Self {
            evm_config,
            pool,
            client,
            config,
            outputs,
            last_emitted_flashblock_id: Arc::default(),
            candidate_source,
        }
    }

    fn previous_flashblock_id(&self) -> FlashblockId {
        *self.last_emitted_flashblock_id.read()
    }

    fn record_emitted_flashblock(&self, block_number: u64, index: u64) {
        *self.last_emitted_flashblock_id.write() = FlashblockId { block_number, index };
    }
}

impl<Pool, Client, S> reth_basic_payload_builder::PayloadBuilder
    for BasePayloadBuilder<Pool, Client, S>
where
    Pool: Clone + Send + Sync,
    Client: Clone + Send + Sync,
    S: Clone + Send + Sync,
{
    type Attributes = BasePayloadBuilderAttributes<BaseTransactionSigned>;
    type BuiltPayload = BaseBuiltPayload;

    fn try_build(
        &self,
        _args: reth_basic_payload_builder::BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        Err(PayloadBuilderError::Other(Box::new(std::io::Error::other(
            "try_build is not supported in flashblocks context",
        ))))
    }

    fn build_empty_payload(
        &self,
        _config: reth_basic_payload_builder::PayloadConfig<
            Self::Attributes,
            reth_basic_payload_builder::HeaderForPayload<Self::BuiltPayload>,
        >,
    ) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        Err(PayloadBuilderError::Other(Box::new(std::io::Error::other(
            "build_empty_payload is not supported in flashblocks context",
        ))))
    }
}

impl<Pool, Client, S> BasePayloadBuilder<Pool, Client, S>
where
    Pool: PoolBounds,
    Client: ClientBounds,
    S: CandidateSource<Pool::Transaction>,
{
    fn get_base_payload_builder_ctx(
        &self,
        config: reth_basic_payload_builder::PayloadConfig<
            BasePayloadBuilderAttributes<base_common_consensus::BaseTxEnvelope>,
        >,
        cancel: CancellationToken,
        extra: FlashblocksExtraCtx,
    ) -> eyre::Result<BasePayloadBuilderCtx> {
        let chain_spec = self.client.chain_spec();
        let timestamp = config.attributes.timestamp();

        let extra_data = if chain_spec.is_jovian_active_at_timestamp(timestamp) {
            config
                .attributes
                .get_jovian_extra_data(chain_spec.base_fee_params_at_timestamp(timestamp))
                .wrap_err("failed to get jovian extra data for flashblocks payload builder")?
        } else if chain_spec.is_holocene_active_at_timestamp(timestamp) {
            config
                .attributes
                .get_holocene_extra_data(chain_spec.base_fee_params_at_timestamp(timestamp))
                .wrap_err("failed to get holocene extra data for flashblocks payload builder")?
        } else {
            Default::default()
        };

        let block_env_attributes = BaseNextBlockEnvAttributes {
            timestamp,
            suggested_fee_recipient: config.attributes.payload_attributes.suggested_fee_recipient,
            prev_randao: config.attributes.payload_attributes.prev_randao,
            gas_limit: config.attributes.gas_limit.unwrap_or(config.parent_header.gas_limit),
            parent_beacon_block_root: config.attributes.payload_attributes.parent_beacon_block_root,
            extra_data,
        };

        let evm_config = self.evm_config.clone();

        let evm_env = evm_config
            .next_evm_env(&config.parent_header, &block_env_attributes)
            .wrap_err("failed to create next evm env")?;

        Ok(BasePayloadBuilderCtx {
            evm_config,
            chain_spec,
            config,
            evm_env,
            block_env_attributes,
            cancel,
            extra,
            builder_config: self.config.clone(),
            rejected_tx_sender: self.outputs.rejected_tx_sender.clone(),
        })
    }

    /// Constructs a Base payload from the transactions sent via the
    /// Payload attributes by the sequencer. If the `no_tx_pool` argument is passed in
    /// the payload attributes, the transaction pool will be ignored and the only transactions
    /// included in the payload will be those sent through the attributes.
    ///
    /// Given build arguments including a Base client, transaction pool,
    /// and configuration, this function creates a transaction payload. Returns
    /// a result indicating success with the payload or an error in case of failure.
    async fn build_payload(
        &self,
        args: BuildArguments<BasePayloadBuilderAttributes<BaseTransactionSigned>, BaseBuiltPayload>,
    ) -> Result<BaseBuiltPayload, PayloadBuilderError> {
        let block_build_start_time = Instant::now();
        let BuildArguments {
            mut cached_reads,
            execution_cache,
            config,
            cancel: block_cancel,
            publish_guard,
        } = args;

        // We log only every Nth block based on sampling ratio to reduce usage
        let block_number = config.parent_header.number + 1;
        let span = if config.parent_header.number.is_multiple_of(self.config.sampling_ratio) {
            span!(Level::INFO, "build_payload", block_number)
        } else {
            tracing::Span::none()
        };
        let _entered = span.enter();
        span.record("payload_id", config.attributes.payload_attributes.id.to_string());

        let timestamp = config.attributes.timestamp();
        let mut ctx = self
            .get_base_payload_builder_ctx(
                config,
                block_cancel.clone(),
                FlashblocksExtraCtx {
                    target_flashblock_count: self.config.flashblocks_per_block(),
                    ..Default::default()
                },
            )
            .map_err(|e| PayloadBuilderError::Other(e.into()))?;

        let mut state_provider = self.client.state_by_block_hash(ctx.parent().hash())?;
        if let Some(execution_cache) = execution_cache {
            state_provider = Box::new(CachedStateProvider::new(
                state_provider,
                execution_cache.cache().clone(),
                Some(CachedStateMetrics::zeroed(CachedStateMetricsSource::Builder)),
            ));
        }
        let db = StateProviderDatabase::new(state_provider);

        // 1. execute the pre steps and seal an early block with that
        let sequencer_tx_start_time = Instant::now();
        let mut state =
            State::builder().with_database(cached_reads.as_db_mut(db)).with_bundle_update().build();

        let mut info = execute_pre_steps(&mut state, &ctx)?;
        let sequencer_tx_time = sequencer_tx_start_time.elapsed();
        BuilderMetrics::sequencer_tx_duration().record(sequencer_tx_time);
        BuilderMetrics::sequencer_tx_gauge().set(sequencer_tx_time);

        // We adjust our flashblocks timings based on time_drift if dynamic adjustment enable
        let (flashblocks_per_block, first_flashblock_offset) =
            self.calculate_flashblocks(timestamp);

        let skip_flashblocks_building = ctx.attributes().no_tx_pool || flashblocks_per_block == 0;

        let prev_flashblock_id = self.previous_flashblock_id();
        let assembly = FlashblockAssembler::build(
            &mut state,
            &ctx,
            &mut info,
            prev_flashblock_id,
            if skip_flashblocks_building { StateRootMode::Compute } else { StateRootMode::Skip },
            FlashblockBaseMode::Include,
        )?;
        let payload = assembly.payload;
        let fb_payload = assembly.flashblock;
        let state_diff = assembly.state_diff;

        self.outputs.payload_tx.send(payload.clone()).await.map_err(PayloadBuilderError::other)?;

        info!(
            target: "payload_builder",
            message = "Fallback block built",
            payload_id = fb_payload.payload_id.to_string(),
        );

        // not emitting flashblock if no_tx_pool in FCU, it's just syncing
        //
        // Published at flashblock_index 0. Regular flashblocks start at
        // index 1, so a client resuming from (block_number, 0) will skip
        // this fallback via the strictly-greater-than comparison in
        // `RingBuffer::entries_after`, but still receive all subsequent
        // flashblocks for the same block.
        if !ctx.attributes().no_tx_pool {
            let flashblock_byte_size = self
                .outputs
                .ws_pub
                .publish(&fb_payload, ctx.block_number(), 0)
                .map_err(PayloadBuilderError::other)?;
            self.record_emitted_flashblock(ctx.block_number(), 0);
            let invalidated = self.pool.invalidate_from_state_diff(&state_diff);
            if invalidated > 0 {
                debug!(
                    target: "payload_builder",
                    invalidated,
                    "transactions invalidated after fallback flashblock publication"
                );
            }
            BuilderMetrics::flashblock_byte_size_histogram().record(flashblock_byte_size as f64);
            BuilderMetrics::first_flashblock_time_offset()
                .record(first_flashblock_offset.as_millis() as f64);
            BuilderMetrics::reduced_flashblocks_number()
                .record(self.config.flashblocks_per_block().saturating_sub(flashblocks_per_block)
                    as f64);
        } else {
            info!(
                target: "payload_builder",
                "No transaction pool, skipping transaction pool processing",
            );
            BuilderMetrics::payload_num_tx().record(info.executed_transactions.len() as f64);
            BuilderMetrics::payload_num_tx_gauge().set(info.executed_transactions.len() as f64);
        }

        // fcu just arrived late, not syncing
        if flashblocks_per_block == 0 && !ctx.attributes().no_tx_pool {
            error!(
                target: "payload_builder",
                message = "FCU arrived too late or system clock are unsynced, building 0 flashblocks",
                timestamp,
            );

            self.record_flashblocks_metrics(
                &ctx,
                &info,
                flashblocks_per_block,
                &span,
                "FCU arrived too late or system clock are unsynced, building 0 flashblocks",
            );
        }

        if skip_flashblocks_building {
            let total_block_building_time = block_build_start_time.elapsed();
            BuilderMetrics::total_block_built_duration().record(total_block_building_time);
            BuilderMetrics::total_block_built_gauge().set(total_block_building_time);

            return Ok(payload);
        }

        info!(
            target: "payload_builder",
            message = "Performed flashblocks timing derivation",
            flashblocks_per_block,
            first_flashblock_offset = first_flashblock_offset.as_millis(),
            flashblocks_interval = self.config.flashblocks_interval.as_millis(),
        );

        let gas_per_batch = ctx.block_gas_limit() / flashblocks_per_block;
        let da_per_batch = ctx
            .builder_config
            .da_config
            .max_da_block_size()
            .map(|da_limit| da_limit / flashblocks_per_block);
        let da_footprint_per_batch =
            info.da_footprint_scalar.map(|_| ctx.block_gas_limit() / flashblocks_per_block);

        let extra = FlashblocksExtraCtx {
            flashblock_index: 1,
            target_flashblock_count: flashblocks_per_block,
            target_gas_for_batch: gas_per_batch,
            target_da_for_batch: da_per_batch,
            target_da_footprint_for_batch: da_footprint_per_batch,
            gas_per_batch,
            da_per_batch,
            da_footprint_per_batch,
        };

        let mut fb_cancel = block_cancel.child_token();
        ctx = ctx.with_cancel(fb_cancel.clone()).with_extra_ctx(extra);

        // Create best_transaction iterator
        let best_txs_attributes = ctx.best_transaction_attributes();
        let mut best_txs = BestFlashblocksTxs::new(
            BestPayloadTransactions::new(self.candidate_source.best_transactions(
                self.pool.best_transactions_with_attributes(best_txs_attributes),
                best_txs_attributes,
            )),
            self.config.rejection_cache.clone(),
        );
        let interval = self.config.flashblocks_interval;
        let (tx, mut rx) = mpsc::channel((self.config.flashblocks_per_block() + 1) as usize);

        tokio::spawn({
            let block_cancel = block_cancel.clone();

            async move {
                let mut timer = tokio::time::interval_at(
                    tokio::time::Instant::now()
                        .checked_add(first_flashblock_offset)
                        .expect("can add flashblock offset to current time"),
                    interval,
                );

                loop {
                    tokio::select! {
                        _ = timer.tick() => {
                            // cancel current payload building job
                            fb_cancel.cancel();
                            fb_cancel = block_cancel.child_token();
                            // this will tick at first_flashblock_offset,
                            // starting the second flashblock
                            if tx.send(fb_cancel.clone()).await.is_err() {
                                // receiver channel was dropped, return.
                                // this will only happen if the `build_payload` function returns,
                                // due to payload building error or the main cancellation token being
                                // cancelled.
                                return;
                            }
                        }
                        _ = block_cancel.cancelled() => {
                            return;
                        }
                    }
                }
            }
        });

        // Highest executed nonce per sender, updated incrementally per flashblock.
        let mut executed_sender_nonces: HashMap<Address, u64> = HashMap::default();

        // Process flashblocks in a blocking loop
        loop {
            let flashblock_index = ctx.flashblock_index();
            let fb_span = if span.is_none() {
                tracing::Span::none()
            } else {
                span!(
                    parent: &span,
                    Level::INFO,
                    "build_flashblock",
                    flashblock_index,
                )
            };
            let _entered = fb_span.enter();

            if ctx.flashblock_index() > ctx.target_flashblock_count() {
                self.record_flashblocks_metrics(
                    &ctx,
                    &info,
                    flashblocks_per_block,
                    &span,
                    "Payload building complete, target flashblock count reached",
                );
                return self.finalize_payload(&mut state, &ctx, &mut info);
            }

            // build first flashblock immediately
            let next_flashblocks_ctx = match self
                .build_next_flashblock(
                    &ctx,
                    &mut info,
                    &mut state,
                    &mut best_txs,
                    &block_cancel,
                    &publish_guard,
                    &fb_span,
                    &mut executed_sender_nonces,
                )
                .await
            {
                Ok(Some(next_flashblocks_ctx)) => next_flashblocks_ctx,
                Ok(None) => {
                    self.record_flashblocks_metrics(
                        &ctx,
                        &info,
                        flashblocks_per_block,
                        &span,
                        "Payload building complete, job cancelled or target flashblock count reached",
                    );
                    return self.finalize_payload(&mut state, &ctx, &mut info);
                }
                Err(err) => {
                    error!(
                        target: "payload_builder",
                        "Failed to build flashblock {} for block number {}: {}",
                        ctx.flashblock_index(),
                        ctx.block_number(),
                        err
                    );
                    return Err(PayloadBuilderError::Other(err.into()));
                }
            };

            tokio::select! {
                Some(fb_cancel) = rx.recv() => {
                    ctx = ctx.with_cancel(fb_cancel).with_extra_ctx(next_flashblocks_ctx);
                },
                _ = block_cancel.cancelled() => {
                    self.record_flashblocks_metrics(
                        &ctx,
                        &info,
                        flashblocks_per_block,
                        &span,
                        "Payload building complete, channel closed or job cancelled",
                    );
                    return self.finalize_payload(&mut state, &ctx, &mut info);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn build_next_flashblock<
        DB: Database<Error = ProviderError> + std::fmt::Debug + AsRef<P> + revm::Database,
        P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
    >(
        &self,
        ctx: &BasePayloadBuilderCtx,
        info: &mut ExecutionInfo,
        state: &mut State<DB>,
        best_txs: &mut NextBestFlashblocksTxs<Pool>,
        block_cancel: &CancellationToken,
        publish_guard: &parking_lot::Mutex<()>,
        span: &tracing::Span,
        executed_sender_nonces: &mut HashMap<Address, u64>,
    ) -> eyre::Result<Option<FlashblocksExtraCtx>> {
        let flashblock_index = ctx.flashblock_index();
        let payload_id = ctx.payload_id().to_string();
        let target_gas_for_batch = ctx.extra.target_gas_for_batch;
        let mut target_da_for_batch = ctx.extra.target_da_for_batch;
        let mut target_da_footprint_for_batch = ctx.extra.target_da_footprint_for_batch;

        info!(
            target: "payload_builder",
            block_number = ctx.block_number(),
            flashblock_index,
            target_gas = target_gas_for_batch,
            gas_used = info.cumulative_gas_used,
            target_da = target_da_for_batch,
            da_used = info.cumulative_da_bytes_used,
            block_gas_used = ctx.block_gas_limit(),
            target_da_footprint = target_da_footprint_for_batch,
            "Building flashblock",
        );
        let flashblock_build_start_time = Instant::now();
        self.emit_flashblock_event(
            ctx,
            &payload_id,
            TransactionEventType::BuilderFlashblockStarted,
            None,
            || {
                BuilderFlashblockStartedEventData::new(
                    target_gas_for_batch,
                    info.cumulative_gas_used,
                    target_da_for_batch,
                    info.cumulative_da_bytes_used,
                    target_da_footprint_for_batch,
                )
            },
        );

        // Correct the pool's sender nonce tracking before reading the next iterator.
        // `prune_transactions` clears sender_info, causing nonce-continuation txs to
        // land in `queued` instead of `pending` since the block isn't sealed yet.
        if !executed_sender_nonces.is_empty() {
            let changed_accounts: Vec<ChangedAccount> = executed_sender_nonces
                .iter()
                .map(|(&address, &nonce)| {
                    // Fall back to zero balance on error — conservatively parks the tx until the next block resolves it.
                    let balance = match state.basic(address) {
                        Ok(Some(info)) => info.balance,
                        Ok(None) => U256::ZERO,
                        Err(e) => {
                            warn!(address = %address, error = %e, "failed to read sender balance from state, defaulting to zero");
                            U256::ZERO
                        }
                    };
                    ChangedAccount { address, nonce: nonce + 1, balance }
                })
                .collect();
            self.pool.update_accounts(changed_accounts);
        }

        let best_txs_start_time = Instant::now();
        let best_txs_attributes = ctx.best_transaction_attributes();
        best_txs.refresh_iterator(BestPayloadTransactions::new(
            self.candidate_source.best_transactions(
                self.pool.best_transactions_with_attributes(best_txs_attributes),
                best_txs_attributes,
            ),
        ));
        let transaction_pool_fetch_time = best_txs_start_time.elapsed();
        BuilderMetrics::transaction_pool_fetch_duration().record(transaction_pool_fetch_time);
        BuilderMetrics::transaction_pool_fetch_gauge().set(transaction_pool_fetch_time);

        let tx_execution_start_time = Instant::now();
        let limits = ResourceLimits {
            block_gas_limit: target_gas_for_batch.min(ctx.block_gas_limit()),
            tx_data_limit: ctx.builder_config.da_config.max_da_tx_size(),
            block_data_limit: target_da_for_batch,
            da_footprint_gas_scalar: info.da_footprint_scalar,
            block_da_footprint_limit: target_da_footprint_for_batch,
            tx_execution_time_limit_us: ctx.builder_config.max_execution_time_per_tx_us,
            block_uncompressed_size_limit: ctx.builder_config.max_uncompressed_block_size,
        };
        let diag = ctx
            .execute_best_transactions(info, state, best_txs, &limits)
            .wrap_err("failed to execute best transactions")?;

        // Evict permanently rejected transactions from the iterator and pool.
        // The rejection cache (inside best_txs) prevents re-entry on P2P re-gossip.
        if !diag.permanently_rejected_txs.is_empty() {
            let rejected_count = diag.permanently_rejected_txs.len();
            best_txs.mark_rejected(&diag.permanently_rejected_txs);
            self.config.metering_provider.remove(&diag.permanently_rejected_txs);
            self.pool.remove_transactions(diag.permanently_rejected_txs.clone());
            info!(
                target: "payload_builder",
                count = rejected_count,
                "evicted permanently rejected transactions from pool",
            );
        }

        // Extract last transactions
        let new_transactions = info.executed_transactions[info.extra.last_flashblock_index..]
            .iter()
            .map(|tx| tx.tx_hash())
            .collect::<Vec<_>>();
        best_txs.mark_committed(&new_transactions);
        self.config.metering_provider.remove(&new_transactions);
        self.pool.prune_transactions(new_transactions);

        // Track executed nonces incrementally for the next flashblock's update_accounts call.
        debug_assert_eq!(
            info.executed_transactions.len(),
            info.executed_senders.len(),
            "executed_transactions and executed_senders must be in lockstep"
        );
        for (tx, sender) in info.executed_transactions[info.extra.last_flashblock_index..]
            .iter()
            .zip(info.executed_senders[info.extra.last_flashblock_index..].iter())
        {
            executed_sender_nonces
                .entry(*sender)
                .and_modify(|n| *n = (*n).max(tx.nonce()))
                .or_insert_with(|| tx.nonce());
        }

        // We got block cancelled, we won't need anything from the block at this point
        // Caution: this assume that block cancel token only cancelled when new FCU is received
        if block_cancel.is_cancelled() {
            self.emit_flashblock_event(
                ctx,
                &payload_id,
                TransactionEventType::BuilderFlashblockBuildStopped,
                None,
                || {
                    BuilderFlashblockStoppedEventData::new(
                        "block_cancelled_before_build",
                        0,
                        flashblock_build_start_time.elapsed().as_secs_f64() * 1000.0,
                    )
                },
            );
            self.record_flashblocks_metrics(
                ctx,
                info,
                ctx.target_flashblock_count(),
                span,
                "Payload building complete, channel closed or job cancelled",
            );
            return Ok(None);
        }

        let payload_transaction_simulation_time = tx_execution_start_time.elapsed();
        BuilderMetrics::payload_transaction_simulation_duration()
            .record(payload_transaction_simulation_time);
        BuilderMetrics::payload_transaction_simulation_gauge()
            .set(payload_transaction_simulation_time);

        let total_block_built_duration = Instant::now();
        let prev_flashblock_id = self.previous_flashblock_id();
        let build_result = FlashblockAssembler::build(
            state,
            ctx,
            info,
            prev_flashblock_id,
            if ctx.attributes().no_tx_pool { StateRootMode::Compute } else { StateRootMode::Skip },
            FlashblockBaseMode::Omit,
        );
        let total_block_built_duration = total_block_built_duration.elapsed();
        BuilderMetrics::total_block_built_duration().record(total_block_built_duration);
        BuilderMetrics::total_block_built_gauge().set(total_block_built_duration);

        match build_result {
            Err(err) => {
                BuilderMetrics::invalid_built_blocks_count().increment(1);
                Err(err).wrap_err("failed to build payload")
            }
            Ok(assembly) => {
                let new_payload = assembly.payload;
                let mut fb_payload = assembly.flashblock;
                let state_diff = assembly.state_diff;
                fb_payload.index = flashblock_index;
                let serialized_flashblock = WebSocketPublisher::serialize(&fb_payload)
                    .wrap_err("failed to serialize flashblock for websocket publication")?;

                // Synchronized check + publish.
                // The publish_guard mutex ensures that if get_payload (resolve_kind) is called,
                // it will either:
                // 1. Cancel before we acquire the lock → we see cancelled and return early
                // 2. Wait for us to release the lock → we publish, then it cancels (correct behavior)
                let (cancelled, flashblock_byte_size) = {
                    let _guard = publish_guard.lock();
                    if block_cancel.is_cancelled() {
                        (true, 0)
                    } else {
                        let size = self.outputs.ws_pub.publish_serialized(
                            serialized_flashblock,
                            ctx.block_number(),
                            flashblock_index,
                        );
                        self.record_emitted_flashblock(ctx.block_number(), flashblock_index);
                        (false, size)
                    }
                };

                if cancelled {
                    self.emit_flashblock_event(
                        ctx,
                        &payload_id,
                        TransactionEventType::BuilderFlashblockBuildStopped,
                        None,
                        || {
                            BuilderFlashblockStoppedEventData::new(
                                "payload_resolved_before_publish",
                                fb_payload.diff.transactions.len(),
                                flashblock_build_start_time.elapsed().as_secs_f64() * 1000.0,
                            )
                        },
                    );
                    self.record_flashblocks_metrics(
                        ctx,
                        info,
                        ctx.target_flashblock_count(),
                        span,
                        "Payload building complete, channel closed or job cancelled",
                    );
                    return Ok(None);
                }

                // Invalidate only after the synchronized publish check accepts
                // this flashblock. An abandoned build must not evict transactions
                // based on state that never became visible.
                let invalidated = self.pool.invalidate_from_state_diff(&state_diff);
                if invalidated > 0 {
                    debug!(
                        target: "payload_builder",
                        invalidated,
                        "transactions invalidated after flashblock publication"
                    );
                }

                // Send to handler outside mutex.
                self.outputs
                    .payload_tx
                    .send(new_payload.clone())
                    .await
                    .wrap_err("failed to send built payload to handler")?;

                // Record flashblock build duration
                let flashblock_build_duration = flashblock_build_start_time.elapsed();
                self.emit_flashblock_event(
                    ctx,
                    &payload_id,
                    TransactionEventType::BuilderFlashblockPublished,
                    Some(fb_payload.diff.block_hash),
                    || {
                        BuilderFlashblockPublishedEventData::new(
                            fb_payload.diff.transactions.len(),
                            flashblock_byte_size,
                            flashblock_build_duration.as_secs_f64() * 1000.0,
                            fb_payload.diff.gas_used,
                            fb_payload.diff.block_hash,
                        )
                    },
                );
                BuilderMetrics::flashblock_build_duration().record(flashblock_build_duration);
                BuilderMetrics::flashblock_byte_size_histogram()
                    .record(flashblock_byte_size as f64);
                BuilderMetrics::flashblock_num_tx_histogram()
                    .record(info.executed_transactions.len() as f64);

                // Update bundle_state for next iteration
                if let Some(da_limit) = ctx.extra.da_per_batch {
                    if let Some(da) = target_da_for_batch.as_mut() {
                        *da += da_limit;
                    } else {
                        error!(
                            "Builder end up in faulty invariant, if da_per_batch is set then total_da_per_batch must be set"
                        );
                    }
                }

                let target_gas_for_batch = ctx.extra.target_gas_for_batch + ctx.extra.gas_per_batch;

                if let (Some(footprint), Some(da_footprint_limit)) =
                    (target_da_footprint_for_batch.as_mut(), ctx.extra.da_footprint_per_batch)
                {
                    *footprint += da_footprint_limit;
                }

                let next_extra = ctx.extra.clone().next(
                    target_gas_for_batch,
                    target_da_for_batch,
                    target_da_footprint_for_batch,
                );

                let gas_headroom_pct = if limits.block_gas_limit > 0 {
                    (limits.block_gas_limit.saturating_sub(info.cumulative_gas_used) as f64
                        / limits.block_gas_limit as f64
                        * 100.0) as u64
                } else {
                    0
                };
                BuilderMetrics::record_flashblock_diagnostics(
                    flashblock_index,
                    &diag,
                    info,
                    &limits,
                );
                info!(
                    target: "payload_builder",
                    message = "Flashblock built",
                    flashblock_index = flashblock_index,
                    selection_outcome = diag.selection_outcome().as_str(),
                    rejection_reasons = ?diag.rejection_reasons(),
                    txs_considered = diag.txs_considered,
                    txs_included = diag.txs_included,
                    txs_rejected = diag.txs_rejected_total(),
                    min_priority_fee_wei = diag.min_priority_fee.unwrap_or(0),
                    current_gas = info.cumulative_gas_used,
                    target_gas = limits.block_gas_limit,
                    gas_headroom_pct = gas_headroom_pct,
                    current_da = info.cumulative_da_bytes_used,
                    target_flashblocks = ctx.target_flashblock_count(),
                );

                Ok(Some(next_extra))
            }
        }
    }

    fn emit_flashblock_event<D, F>(
        &self,
        ctx: &BasePayloadBuilderCtx,
        payload_id: &str,
        event_type: TransactionEventType,
        block_hash: Option<B256>,
        data: F,
    ) where
        D: Serialize,
        F: FnOnce() -> D,
    {
        if GlobalTransactionEventWriter::get().is_none() {
            return;
        }
        let event_ctx = BuilderTransactionEventContext {
            payload_id: payload_id.to_string(),
            block_number: ctx.block_number(),
            block_hash,
            parent_hash: ctx.parent_hash(),
            flashblock_index: Some(ctx.flashblock_index()),
            target_flashblock_count: ctx.target_flashblock_count(),
            ordering_position: None,
            builder_mode: "flashblocks",
            source_queue: "flashblock_builder",
        };
        emit_builder_payload_event(event_ctx, event_type, data);
    }

    /// Do some logging and metric recording when we stop build flashblocks
    fn record_flashblocks_metrics(
        &self,
        ctx: &BasePayloadBuilderCtx,
        info: &ExecutionInfo,
        flashblocks_per_block: u64,
        span: &tracing::Span,
        message: &str,
    ) {
        BuilderMetrics::block_built_success().increment(1);
        BuilderMetrics::flashblock_count().record(ctx.flashblock_index() as f64);
        BuilderMetrics::missing_flashblocks_count()
            .record(flashblocks_per_block.saturating_sub(ctx.flashblock_index()) as f64);
        BuilderMetrics::payload_num_tx().record(info.executed_transactions.len() as f64);
        BuilderMetrics::payload_num_tx_gauge().set(info.executed_transactions.len() as f64);

        // Record cumulative uncompressed block size
        BuilderMetrics::block_uncompressed_size().record(info.cumulative_uncompressed_bytes as f64);

        debug!(
            target: "payload_builder",
            message = message,
            flashblocks_per_block = flashblocks_per_block,
            flashblock_index = ctx.flashblock_index(),
        );

        span.record("flashblock_count", ctx.flashblock_index());
    }

    /// Finalize the payload by computing the state root.
    fn finalize_payload<DB, P>(
        &self,
        state: &mut State<DB>,
        ctx: &BasePayloadBuilderCtx,
        info: &mut ExecutionInfo,
    ) -> Result<BaseBuiltPayload, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError> + AsRef<P>,
        P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
    {
        let start_time = Instant::now();

        // Build the final block WITH state root computed
        let final_payload = FlashblockAssembler::build(
            state,
            ctx,
            info,
            FlashblockId::default(),
            StateRootMode::Compute,
            FlashblockBaseMode::Omit,
        )?
        .payload;

        ctx.flush_rejected_txs(info);
        self.emit_final_inclusion_events(ctx, &final_payload);

        let elapsed = start_time.elapsed();
        info!(
            target: "payload_builder",
            block_number = ctx.block_number(),
            block_hash = ?final_payload.block().hash(),
            elapsed_ms = elapsed.as_millis(),
            "Finalized payload with state root"
        );

        Ok(final_payload)
    }

    fn emit_final_inclusion_events(
        &self,
        ctx: &BasePayloadBuilderCtx,
        final_payload: &BaseBuiltPayload,
    ) {
        if GlobalTransactionEventWriter::get().is_none() {
            return;
        }

        let block = final_payload.block();
        let block_hash = block.hash();
        let block_number = block.number;
        let transaction_count = block.body().transactions.len();
        let payload_event_ctx = BuilderTransactionEventContext {
            payload_id: ctx.payload_id().to_string(),
            block_number,
            block_hash: Some(block_hash),
            parent_hash: ctx.parent_hash(),
            flashblock_index: None,
            target_flashblock_count: ctx.target_flashblock_count(),
            ordering_position: None,
            builder_mode: "flashblocks",
            source_queue: "finalized_payload",
        };
        emit_builder_payload_event(
            payload_event_ctx.clone(),
            TransactionEventType::BuilderPayloadFinalized,
            || {
                BuilderPayloadFinalizedEventData::new(
                    transaction_count,
                    block.gas_used,
                    block.gas_limit,
                    block.timestamp,
                    "builder_finalized_payload",
                )
            },
        );

        for (position, tx) in block.body().transactions.iter().enumerate() {
            let mut event_ctx = payload_event_ctx.clone();
            event_ctx.ordering_position = Some(position as u64);
            emit_builder_transaction_event(
                event_ctx,
                TransactionEventType::BuilderIncluded,
                tx.tx_hash(),
                || BuilderIncludedEventData::new("builder_finalized_payload"),
            );
        }
    }

    /// Calculate number of flashblocks, taking time drift into account.
    pub(super) fn calculate_flashblocks(&self, timestamp: u64) -> (u64, Duration) {
        // We use this system time to determine remaining time to build a block
        // Things to consider:
        // FCU(a) - FCU with attributes
        // FCU(a) could arrive with `block_time - fb_time < delay`. In this case we could only produce 1 flashblock
        // FCU(a) could arrive with `delay < fb_time` - in this case we will shrink first flashblock
        // FCU(a) could arrive with `fb_time < delay < block_time - fb_time` - in this case we will issue less flashblocks
        let target_time = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(timestamp)
            - self.config.flashblocks_leeway_time;
        let now = std::time::SystemTime::now();
        let Some(time_drift) =
            target_time.duration_since(now).ok().filter(|duration| duration.as_millis() > 0)
        else {
            // in this case, we have no time to produce any flashblocks
            return (0, Duration::ZERO);
        };

        BuilderMetrics::flashblocks_time_drift().record(
            self.config.block_time.as_millis().saturating_sub(time_drift.as_millis()) as f64,
        );
        debug!(
            target: "payload_builder",
            message = "Time drift for building round",
            ?target_time,
            time_drift = self.config.block_time.as_millis().saturating_sub(time_drift.as_millis()),
            ?timestamp
        );
        // This is extra check to ensure that we would account at least for block time in case we have any timer discrepancies.
        let time_drift = time_drift.min(self.config.block_time);
        let interval = self.config.flashblocks_interval.as_millis() as u64;
        let time_drift = time_drift.as_millis() as u64;
        let first_flashblock_offset = time_drift.rem(interval);
        if first_flashblock_offset == 0 {
            // We have perfect division, so we use interval as first fb offset
            (time_drift.div(interval), Duration::from_millis(interval))
        } else {
            // Non-perfect division, so we account for it.
            (time_drift.div(interval) + 1, Duration::from_millis(first_flashblock_offset))
        }
    }
}

#[async_trait::async_trait]
impl<Pool, Client, S> PayloadBuilder for BasePayloadBuilder<Pool, Client, S>
where
    Pool: PoolBounds,
    Client: ClientBounds,
    S: CandidateSource<Pool::Transaction> + Clone + Unpin + 'static,
{
    type Attributes = BasePayloadBuilderAttributes<BaseTransactionSigned>;
    type BuiltPayload = BaseBuiltPayload;

    async fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
        payload_tx: &watch::Sender<Option<Self::BuiltPayload>>,
    ) -> Result<(), PayloadBuilderError> {
        // Keep construction behind this call boundary so its state provider, including any shared
        // cache handle, is released before publishing wakes the payload resolver.
        let payload = self.build_payload(args).await?;
        payload_tx.send_replace(Some(payload));
        Ok(())
    }
}

pub(crate) fn execute_pre_steps<DB>(
    state: &mut State<DB>,
    ctx: &BasePayloadBuilderCtx,
) -> Result<ExecutionInfo, PayloadBuilderError>
where
    DB: Database<Error = ProviderError> + std::fmt::Debug + revm::Database,
{
    // 1. apply pre-execution changes
    ctx.evm_config
        .builder_for_next_block(state, ctx.parent(), ctx.block_env_attributes.clone())
        .map_err(PayloadBuilderError::other)?
        .apply_pre_execution_changes()?;

    // 2. execute sequencer transactions
    let info = ctx.execute_sequencer_transactions(state)?;

    Ok(info)
}
#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256};
    use reth_execution_cache::{CachedStateProvider, CachedStatus, ExecutionCache, SavedCache};
    use reth_provider::{StateProviderBox, noop::NoopProvider};

    #[test]
    fn canonical_state_provider_uses_shared_cache_without_filling_misses() {
        let address = Address::random();
        let cached_key = B256::random();
        let uncached_key = B256::random();
        let cached_value = U256::from(1);
        let uncached_value = U256::from(2);
        let cache = SavedCache::new(B256::ZERO, ExecutionCache::new(1_000));
        cache.cache().insert_storage(address, cached_key, Some(cached_value));

        {
            let state_provider = Box::new(CachedStateProvider::new(
                Box::new(NoopProvider::default()) as StateProviderBox,
                cache.cache().clone(),
                None,
            )) as StateProviderBox;

            assert!(!cache.is_available(), "provider must hold the shared cache while in use");
            assert_eq!(state_provider.storage(address, cached_key).unwrap(), Some(cached_value));
            assert_eq!(state_provider.storage(address, uncached_key).unwrap(), None);
            assert_eq!(
                cache
                    .cache()
                    .get_or_try_insert_storage_with(address, uncached_key, || Ok::<_, ()>(
                        uncached_value
                    ))
                    .unwrap(),
                CachedStatus::NotCached(uncached_value),
                "lookup-only canonical reads must not fill cache misses"
            );
        }

        assert!(cache.is_available(), "provider must release the shared cache when dropped");
    }
}
