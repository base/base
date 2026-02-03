use core::time::Duration;
use std::{
    ops::{Div, Rem},
    sync::Arc,
    time::Instant,
};

use alloy_consensus::{
    BlockBody, EMPTY_OMMER_ROOT_HASH, Header, constants::EMPTY_WITHDRAWALS, proofs,
};
use alloy_eips::{Encodable2718, eip7685::EMPTY_REQUESTS_HASH, merge::BEACON_NONCE};
use alloy_evm::Database;
use alloy_primitives::{B256, U256};
use base_access_lists::{FlashblockAccessList, FlashblockAccessListBuilder};
use base_flashtypes::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, FlashblocksPayloadV1,
};
use either::Either;
use eyre::WrapErr as _;
use reth_basic_payload_builder::BuildOutcome;
use reth_evm::{ConfigureEvm, execute::BlockBuilder};
use reth_node_api::{Block, BuiltPayloadExecutedBlock, PayloadBuilderError};
use reth_optimism_consensus::{calculate_receipt_root_no_memo_optimism, isthmus};
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_forks::OpHardforks;
use reth_optimism_node::{OpBuiltPayload, OpPayloadBuilderAttributes};
use reth_optimism_primitives::OpTransactionSigned;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_payload_util::BestPayloadTransactions;
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{
    BlockExecutionOutput, BlockExecutionResult, ExecutionOutcome, HashedPostStateProvider,
    ProviderError, StateRootProvider, StorageRootProvider,
};
use reth_revm::{
    State, database::StateProviderDatabase, db::states::bundle_state::BundleRetention,
};
use reth_transaction_pool::TransactionPool;
use reth_trie::{HashedPostState, updates::TrieUpdates};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, metadata::Level, span, warn};

use super::{state_root_task::StateRootTaskHandle, wspub::WebSocketPublisher};
use crate::{
    flashblocks::{
        BuilderConfig, FlashblocksExtraCtx,
        best_txs::BestFlashblocksTxs,
        config::FlashBlocksConfigExt,
        context::OpPayloadBuilderCtx,
        generator::{BlockCell, BuildArguments, PayloadBuilder},
    },
    metrics::OpRBuilderMetrics,
    primitives::reth::ExecutionInfo,
    traits::{ClientBounds, PoolBounds},
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

#[derive(Debug, Default, Clone)]
pub struct FlashblocksExecutionInfo {
    /// Index of the last consumed flashblock
    pub(crate) last_flashblock_index: usize,

    /// Flashblock-level access list builder
    pub(crate) access_list_builder: FlashblockAccessListBuilder,
}

/// Optimism's payload builder
#[derive(Debug, Clone)]
pub(super) struct OpPayloadBuilder<Pool, Client> {
    /// The type responsible for creating the evm.
    pub evm_config: OpEvmConfig,
    /// The transaction pool
    pub pool: Pool,
    /// Node client
    pub client: Client,
    /// Sender for sending built payloads to [`PayloadHandler`],
    /// which broadcasts outgoing payloads via p2p.
    pub payload_tx: mpsc::Sender<OpBuiltPayload>,
    /// WebSocket publisher for broadcasting flashblocks
    /// to all connected subscribers.
    pub ws_pub: Arc<WebSocketPublisher>,
    /// System configuration for the builder
    pub config: BuilderConfig,
    /// The metrics for the builder
    pub metrics: Arc<OpRBuilderMetrics>,
}

impl<Pool, Client> OpPayloadBuilder<Pool, Client> {
    /// `OpPayloadBuilder` constructor.
    pub(super) const fn new(
        evm_config: OpEvmConfig,
        pool: Pool,
        client: Client,
        config: BuilderConfig,
        payload_tx: mpsc::Sender<OpBuiltPayload>,
        ws_pub: Arc<WebSocketPublisher>,
        metrics: Arc<OpRBuilderMetrics>,
    ) -> Self {
        Self { evm_config, pool, client, payload_tx, ws_pub, config, metrics }
    }
}

impl<Pool, Client> reth_basic_payload_builder::PayloadBuilder for OpPayloadBuilder<Pool, Client>
where
    Pool: Clone + Send + Sync,
    Client: Clone + Send + Sync,
{
    type Attributes = OpPayloadBuilderAttributes<OpTransactionSigned>;
    type BuiltPayload = OpBuiltPayload;

    fn try_build(
        &self,
        _args: reth_basic_payload_builder::BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        unimplemented!()
    }

    fn build_empty_payload(
        &self,
        _config: reth_basic_payload_builder::PayloadConfig<
            Self::Attributes,
            reth_basic_payload_builder::HeaderForPayload<Self::BuiltPayload>,
        >,
    ) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        unimplemented!()
    }
}

impl<Pool, Client> OpPayloadBuilder<Pool, Client>
where
    Pool: PoolBounds,
    Client: ClientBounds,
{
    fn get_op_payload_builder_ctx(
        &self,
        config: reth_basic_payload_builder::PayloadConfig<
            OpPayloadBuilderAttributes<op_alloy_consensus::OpTxEnvelope>,
        >,
        cancel: CancellationToken,
        extra: FlashblocksExtraCtx,
    ) -> eyre::Result<OpPayloadBuilderCtx> {
        let chain_spec = self.client.chain_spec();
        let timestamp = config.attributes.timestamp();

        let extra_data = if chain_spec.is_jovian_active_at_timestamp(timestamp) {
            config
                .attributes
                .get_jovian_extra_data(chain_spec.base_fee_params_at_timestamp(timestamp))
                .wrap_err("failed to get holocene extra data for flashblocks payload builder")?
        } else if chain_spec.is_holocene_active_at_timestamp(timestamp) {
            config
                .attributes
                .get_holocene_extra_data(chain_spec.base_fee_params_at_timestamp(timestamp))
                .wrap_err("failed to get holocene extra data for flashblocks payload builder")?
        } else {
            Default::default()
        };

        let block_env_attributes = OpNextBlockEnvAttributes {
            timestamp,
            suggested_fee_recipient: config.attributes.suggested_fee_recipient(),
            prev_randao: config.attributes.prev_randao(),
            gas_limit: config.attributes.gas_limit.unwrap_or(config.parent_header.gas_limit),
            parent_beacon_block_root: config.attributes.payload_attributes.parent_beacon_block_root,
            extra_data,
        };

        let evm_config = self.evm_config.clone();

        let evm_env = evm_config
            .next_evm_env(&config.parent_header, &block_env_attributes)
            .wrap_err("failed to create next evm env")?;

        Ok(OpPayloadBuilderCtx {
            evm_config,
            chain_spec,
            config,
            evm_env,
            block_env_attributes,
            cancel,
            da_config: self.config.da_config.clone(),
            gas_limit_config: self.config.gas_limit_config.clone(),
            metrics: Default::default(),
            extra,
            max_gas_per_txn: self.config.max_gas_per_txn,
            tx_data_store: self.config.tx_data_store.clone(),
        })
    }

    /// Constructs an Optimism payload from the transactions sent via the
    /// Payload attributes by the sequencer. If the `no_tx_pool` argument is passed in
    /// the payload attributes, the transaction pool will be ignored and the only transactions
    /// included in the payload will be those sent through the attributes.
    ///
    /// Given build arguments including an Optimism client, transaction pool,
    /// and configuration, this function creates a transaction payload. Returns
    /// a result indicating success with the payload or an error in case of failure.
    async fn build_payload(
        &self,
        args: BuildArguments<OpPayloadBuilderAttributes<OpTransactionSigned>, OpBuiltPayload>,
        best_payload: BlockCell<OpBuiltPayload>,
    ) -> Result<(), PayloadBuilderError> {
        let block_build_start_time = Instant::now();
        let BuildArguments {
            mut cached_reads,
            config,
            cancel: block_cancel,
            finalized_cell,
            compute_state_root_on_finalize,
            publish_guard,
        } = args;

        // We log only every Nth block based on sampling ratio to reduce usage
        let span = if config.parent_header.number.is_multiple_of(self.config.sampling_ratio) {
            span!(Level::INFO, "build_payload")
        } else {
            tracing::Span::none()
        };
        let _entered = span.enter();
        span.record("payload_id", config.attributes.payload_attributes.id.to_string());

        let timestamp = config.attributes.timestamp();
        let disable_state_root = self.config.flashblocks.disable_state_root;
        let ctx = self
            .get_op_payload_builder_ctx(
                config.clone(),
                block_cancel.clone(),
                FlashblocksExtraCtx {
                    target_flashblock_count: self.config.flashblocks_per_block(),
                    disable_state_root,
                    ..Default::default()
                },
            )
            .map_err(|e| PayloadBuilderError::Other(e.into()))?;

        let state_provider = self.client.state_by_block_hash(ctx.parent().hash())?;
        let db = StateProviderDatabase::new(state_provider);

        // 1. execute the pre steps and seal an early block with that
        let sequencer_tx_start_time = Instant::now();
        let mut state =
            State::builder().with_database(cached_reads.as_db_mut(db)).with_bundle_update().build();

        let mut info = execute_pre_steps(&mut state, &ctx)?;
        let sequencer_tx_time = sequencer_tx_start_time.elapsed();
        ctx.metrics.sequencer_tx_duration.record(sequencer_tx_time);
        ctx.metrics.sequencer_tx_gauge.set(sequencer_tx_time);

        let (payload, fb_payload) = build_block(
            &mut state,
            &ctx,
            &mut info,
            !disable_state_root || ctx.attributes().no_tx_pool, // need to calculate state root for CL sync
        )?;

        self.payload_tx.send(payload.clone()).await.map_err(PayloadBuilderError::other)?;
        best_payload.set(payload);

        info!(
            target: "payload_builder",
            message = "Fallback block built",
            payload_id = fb_payload.payload_id.to_string(),
        );

        // not emitting flashblock if no_tx_pool in FCU, it's just syncing
        if !ctx.attributes().no_tx_pool {
            let flashblock_byte_size =
                self.ws_pub.publish(&fb_payload).map_err(PayloadBuilderError::other)?;
            ctx.metrics.flashblock_byte_size_histogram.record(flashblock_byte_size as f64);
        }

        if ctx.attributes().no_tx_pool {
            info!(
                target: "payload_builder",
                "No transaction pool, skipping transaction pool processing",
            );

            let total_block_building_time = block_build_start_time.elapsed();
            ctx.metrics.total_block_built_duration.record(total_block_building_time);
            ctx.metrics.total_block_built_gauge.set(total_block_building_time);
            ctx.metrics.payload_num_tx.record(info.executed_transactions.len() as f64);
            ctx.metrics.payload_num_tx_gauge.set(info.executed_transactions.len() as f64);

            // return early since we don't need to build a block with transactions from the pool
            return Ok(());
        }
        // We adjust our flashblocks timings based on time_drift if dynamic adjustment enable
        let (flashblocks_per_block, first_flashblock_offset) =
            self.calculate_flashblocks(timestamp);
        info!(
            target: "payload_builder",
            message = "Performed flashblocks timing derivation",
            flashblocks_per_block,
            first_flashblock_offset = first_flashblock_offset.as_millis(),
            flashblocks_interval = self.config.flashblocks.interval.as_millis(),
        );
        ctx.metrics.reduced_flashblocks_number.record(
            self.config.flashblocks_per_block().saturating_sub(ctx.target_flashblock_count())
                as f64,
        );
        ctx.metrics.first_flashblock_time_offset.record(first_flashblock_offset.as_millis() as f64);
        let gas_per_batch = ctx.block_gas_limit() / flashblocks_per_block;
        let da_per_batch =
            ctx.da_config.max_da_block_size().map(|da_limit| da_limit / flashblocks_per_block);
        let da_footprint_per_batch =
            info.da_footprint_scalar.map(|_| ctx.block_gas_limit() / flashblocks_per_block);

        let extra = FlashblocksExtraCtx {
            flashblock_index: 1,
            target_flashblock_count: flashblocks_per_block,
            target_gas_for_batch: gas_per_batch,
            target_da_for_batch: da_per_batch,
            gas_per_batch,
            da_per_batch,
            da_footprint_per_batch,
            disable_state_root,
            target_da_footprint_for_batch: da_footprint_per_batch,
        };

        let mut fb_cancel = block_cancel.child_token();
        let mut ctx = self
            .get_op_payload_builder_ctx(config, fb_cancel.clone(), extra)
            .map_err(|e| PayloadBuilderError::Other(e.into()))?;

        // Create best_transaction iterator
        let mut best_txs = BestFlashblocksTxs::new(BestPayloadTransactions::new(
            self.pool.best_transactions_with_attributes(ctx.best_transaction_attributes()),
        ));
        let interval = self.config.flashblocks.interval;
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

        // Process flashblocks in a blocking loop
        loop {
            let fb_span = if span.is_none() {
                tracing::Span::none()
            } else {
                span!(
                    parent: &span,
                    Level::INFO,
                    "build_flashblock",
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
                if compute_state_root_on_finalize {
                    self.finalize_payload(&mut state, &ctx, &mut info, &finalized_cell)?;
                }
                return Ok(());
            }

            // build first flashblock immediately
            let next_flashblocks_ctx = match self
                .build_next_flashblock(
                    &ctx,
                    &mut info,
                    &mut state,
                    &mut best_txs,
                    &block_cancel,
                    &best_payload,
                    &publish_guard,
                    &fb_span,
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
                    if compute_state_root_on_finalize {
                        self.finalize_payload(&mut state, &ctx, &mut info, &finalized_cell)?;
                    }
                    return Ok(());
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
                    if compute_state_root_on_finalize {
                        self.finalize_payload(&mut state, &ctx, &mut info, &finalized_cell)?;
                    }
                    return Ok(());
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
        ctx: &OpPayloadBuilderCtx,
        info: &mut ExecutionInfo,
        state: &mut State<DB>,
        best_txs: &mut NextBestFlashblocksTxs<Pool>,
        block_cancel: &CancellationToken,
        best_payload: &BlockCell<OpBuiltPayload>,
        publish_guard: &parking_lot::Mutex<()>,
        span: &tracing::Span,
    ) -> eyre::Result<Option<FlashblocksExtraCtx>> {
        let flashblock_index = ctx.flashblock_index();
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

        let best_txs_start_time = Instant::now();
        best_txs.refresh_iterator(BestPayloadTransactions::new(
            self.pool.best_transactions_with_attributes(ctx.best_transaction_attributes()),
        ));
        let transaction_pool_fetch_time = best_txs_start_time.elapsed();
        ctx.metrics.transaction_pool_fetch_duration.record(transaction_pool_fetch_time);
        ctx.metrics.transaction_pool_fetch_gauge.set(transaction_pool_fetch_time);

        let tx_execution_start_time = Instant::now();
        ctx.execute_best_transactions(
            info,
            state,
            best_txs,
            target_gas_for_batch.min(ctx.block_gas_limit()),
            target_da_for_batch,
            target_da_footprint_for_batch,
        )
        .wrap_err("failed to execute best transactions")?;
        // Extract last transactions
        let new_transactions = info.executed_transactions[info.extra.last_flashblock_index..]
            .to_vec()
            .iter()
            .map(|tx| tx.tx_hash())
            .collect::<Vec<_>>();
        best_txs.mark_commited(new_transactions);

        // We got block cancelled, we won't need anything from the block at this point
        // Caution: this assume that block cancel token only cancelled when new FCU is received
        if block_cancel.is_cancelled() {
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
        ctx.metrics
            .payload_transaction_simulation_duration
            .record(payload_transaction_simulation_time);
        ctx.metrics.payload_transaction_simulation_gauge.set(payload_transaction_simulation_time);

        let total_block_built_duration = Instant::now();
        let build_result = build_block(
            state,
            ctx,
            info,
            !ctx.extra.disable_state_root || ctx.attributes().no_tx_pool,
        );
        let total_block_built_duration = total_block_built_duration.elapsed();
        ctx.metrics.total_block_built_duration.record(total_block_built_duration);
        ctx.metrics.total_block_built_gauge.set(total_block_built_duration);

        match build_result {
            Err(err) => {
                ctx.metrics.invalid_built_blocks_count.increment(1);
                Err(err).wrap_err("failed to build payload")
            }
            Ok((new_payload, mut fb_payload)) => {
                fb_payload.index = flashblock_index;
                fb_payload.base = None;

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
                        let size = self
                            .ws_pub
                            .publish(&fb_payload)
                            .wrap_err("failed to publish flashblock via websocket")?;
                        (false, size)
                    }
                };

                if cancelled {
                    self.record_flashblocks_metrics(
                        ctx,
                        info,
                        ctx.target_flashblock_count(),
                        span,
                        "Payload building complete, channel closed or job cancelled",
                    );
                    return Ok(None);
                }

                // Send to handler and set best_payload outside mutex.
                self.payload_tx
                    .send(new_payload.clone())
                    .await
                    .wrap_err("failed to send built payload to handler")?;
                best_payload.set(new_payload);

                // Record flashblock build duration
                ctx.metrics.flashblock_build_duration.record(flashblock_build_start_time.elapsed());
                ctx.metrics.flashblock_byte_size_histogram.record(flashblock_byte_size as f64);
                ctx.metrics
                    .flashblock_num_tx_histogram
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

                info!(
                    target: "payload_builder",
                    message = "Flashblock built",
                    flashblock_index = flashblock_index,
                    current_gas = info.cumulative_gas_used,
                    current_da = info.cumulative_da_bytes_used,
                    target_flashblocks = ctx.target_flashblock_count(),
                );

                Ok(Some(next_extra))
            }
        }
    }

    /// Do some logging and metric recording when we stop build flashblocks
    fn record_flashblocks_metrics(
        &self,
        ctx: &OpPayloadBuilderCtx,
        info: &ExecutionInfo,
        flashblocks_per_block: u64,
        span: &tracing::Span,
        message: &str,
    ) {
        ctx.metrics.block_built_success.increment(1);
        ctx.metrics.flashblock_count.record(ctx.flashblock_index() as f64);
        ctx.metrics
            .missing_flashblocks_count
            .record(flashblocks_per_block.saturating_sub(ctx.flashblock_index()) as f64);
        ctx.metrics.payload_num_tx.record(info.executed_transactions.len() as f64);
        ctx.metrics.payload_num_tx_gauge.set(info.executed_transactions.len() as f64);

        debug!(
            target: "payload_builder",
            message = message,
            flashblocks_per_block = flashblocks_per_block,
            flashblock_index = ctx.flashblock_index(),
        );

        span.record("flashblock_count", ctx.flashblock_index());
    }

    /// Finalize the payload by computing the state root and setting the finalized cell.
    fn finalize_payload<DB, P>(
        &self,
        state: &mut State<DB>,
        ctx: &OpPayloadBuilderCtx,
        info: &mut ExecutionInfo,
        finalized_cell: &BlockCell<OpBuiltPayload>,
    ) -> Result<(), PayloadBuilderError>
    where
        DB: Database<Error = ProviderError> + AsRef<P>,
        P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
    {
        let start_time = Instant::now();

        // Build the final block WITH state root computed
        let (final_payload, _) = build_block(state, ctx, info, true)?;

        let elapsed = start_time.elapsed();
        info!(
            target: "payload_builder",
            block_number = ctx.block_number(),
            block_hash = ?final_payload.block().hash(),
            elapsed_ms = elapsed.as_millis(),
            "Finalized payload with state root"
        );

        finalized_cell.set(final_payload);

        Ok(())
    }

    /// Finalize the payload using streaming state root calculation.
    ///
    /// This method uses a pre-started StateRootTaskHandle to compute the state root
    /// with reduced latency by leveraging pre-fetched trie nodes.
    #[allow(dead_code)]
    async fn finalize_payload_with_streaming<DB, P>(
        &self,
        state: &mut State<DB>,
        ctx: &OpPayloadBuilderCtx,
        info: &mut ExecutionInfo,
        finalized_cell: &BlockCell<OpBuiltPayload>,
        state_root_handle: &StateRootTaskHandle,
    ) -> Result<(), PayloadBuilderError>
    where
        DB: Database<Error = ProviderError> + AsRef<P>,
        P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
    {
        let start_time = Instant::now();

        // Request the state root from the streaming task
        let state_root_result = state_root_handle
            .compute_root()
            .await
            .map_err(|e| PayloadBuilderError::Other(Box::new(e)))?;

        // Build the final block with the pre-computed state root
        let (final_payload, _) = build_block_with_state_root(
            state,
            ctx,
            info,
            state_root_result.state_root,
            state_root_result.trie_updates,
            state_root_result.hashed_state,
        )?;

        let elapsed = start_time.elapsed();
        info!(
            target: "payload_builder",
            block_number = ctx.block_number(),
            block_hash = ?final_payload.block().hash(),
            elapsed_ms = elapsed.as_millis(),
            "Finalized payload with streaming state root"
        );

        finalized_cell.set(final_payload);

        // Shutdown the state root task
        let _ = state_root_handle.shutdown().await;

        Ok(())
    }

    /// Calculate number of flashblocks.
    /// If dynamic is enabled this function will take time drift into the account.
    pub(super) fn calculate_flashblocks(&self, timestamp: u64) -> (u64, Duration) {
        if self.config.flashblocks.fixed {
            return (
                self.config.flashblocks_per_block(),
                // We adjust first FB to ensure that we have at least some time to make all FB in time
                self.config.flashblocks.interval - self.config.flashblocks.leeway_time,
            );
        }

        // We use this system time to determine remining time to build a block
        // Things to consider:
        // FCU(a) - FCU with attributes
        // FCU(a) could arrive with `block_time - fb_time < delay`. In this case we could only produce 1 flashblock
        // FCU(a) could arrive with `delay < fb_time` - in this case we will shrink first flashblock
        // FCU(a) could arrive with `fb_time < delay < block_time - fb_time` - in this case we will issue less flashblocks
        let target_time = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(timestamp)
            - self.config.flashblocks.leeway_time;
        let now = std::time::SystemTime::now();
        let Ok(time_drift) = target_time.duration_since(now) else {
            error!(
                target: "payload_builder",
                message = "FCU arrived too late or system clock are unsynced",
                ?target_time,
                ?now,
            );
            return (self.config.flashblocks_per_block(), self.config.flashblocks.interval);
        };
        self.metrics.flashblocks_time_drift.record(
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
        let interval = self.config.flashblocks.interval.as_millis() as u64;
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
impl<Pool, Client> PayloadBuilder for OpPayloadBuilder<Pool, Client>
where
    Pool: PoolBounds,
    Client: ClientBounds,
{
    type Attributes = OpPayloadBuilderAttributes<OpTransactionSigned>;
    type BuiltPayload = OpBuiltPayload;

    async fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
        best_payload: BlockCell<Self::BuiltPayload>,
    ) -> Result<(), PayloadBuilderError> {
        self.build_payload(args, best_payload).await
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct FlashblocksMetadata {
    /// The block number this flashblock belongs to
    block_number: u64,
    /// The flashblock access list
    access_list: FlashblockAccessList,
}

fn execute_pre_steps<DB>(
    state: &mut State<DB>,
    ctx: &OpPayloadBuilderCtx,
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

pub(super) fn build_block<DB, P>(
    state: &mut State<DB>,
    ctx: &OpPayloadBuilderCtx,
    info: &mut ExecutionInfo,
    calculate_state_root: bool,
) -> Result<(OpBuiltPayload, FlashblocksPayloadV1), PayloadBuilderError>
where
    DB: Database<Error = ProviderError> + AsRef<P> + revm::Database,
    P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
{
    // We use it to preserve state, so we run merge_transitions on transition state at most once
    let untouched_transition_state = state.transition_state.clone();
    let state_merge_start_time = Instant::now();
    state.merge_transitions(BundleRetention::Reverts);
    let state_transition_merge_time = state_merge_start_time.elapsed();
    ctx.metrics.state_transition_merge_duration.record(state_transition_merge_time);
    ctx.metrics.state_transition_merge_gauge.set(state_transition_merge_time);

    let block_number = ctx.block_number();
    assert_eq!(block_number, ctx.parent().number + 1);

    let execution_outcome = ExecutionOutcome::new(
        state.bundle_state.clone(),
        vec![info.receipts.clone()],
        block_number,
        vec![],
    );

    let receipts_root = execution_outcome
        .generic_receipts_root_slow(block_number, |receipts| {
            calculate_receipt_root_no_memo_optimism(
                receipts,
                &ctx.chain_spec,
                ctx.attributes().timestamp(),
            )
        })
        .expect("Number is in range");
    let logs_bloom = execution_outcome.block_logs_bloom(block_number).expect("Number is in range");

    // TODO: maybe recreate state with bundle in here
    // calculate the state root
    let state_root_start_time = Instant::now();
    let mut state_root = B256::ZERO;
    let mut trie_output = TrieUpdates::default();
    let mut hashed_state = HashedPostState::default();

    if calculate_state_root {
        let state_provider = state.database.as_ref();
        hashed_state = state_provider.hashed_post_state(execution_outcome.state());
        (state_root, trie_output) = {
            state.database.as_ref().state_root_with_updates(hashed_state.clone()).inspect_err(
                |err| {
                    warn!(target: "payload_builder",
                    parent_header=%ctx.parent().hash(),
                        %err,
                        "failed to calculate state root for payload"
                    );
                },
            )?
        };
        let state_root_calculation_time = state_root_start_time.elapsed();
        ctx.metrics.state_root_calculation_duration.record(state_root_calculation_time);
        ctx.metrics.state_root_calculation_gauge.set(state_root_calculation_time);
    }

    let mut requests_hash = None;
    let withdrawals_root =
        if ctx.chain_spec.is_isthmus_active_at_timestamp(ctx.attributes().timestamp()) {
            // always empty requests hash post isthmus
            requests_hash = Some(EMPTY_REQUESTS_HASH);

            // withdrawals root field in block header is used for storage root of L2 predeploy
            // `l2tol1-message-passer`
            Some(
                isthmus::withdrawals_root(execution_outcome.state(), state.database.as_ref())
                    .map_err(PayloadBuilderError::other)?,
            )
        } else if ctx.chain_spec.is_canyon_active_at_timestamp(ctx.attributes().timestamp()) {
            Some(EMPTY_WITHDRAWALS)
        } else {
            None
        };

    // create the block header
    let transactions_root = proofs::calculate_transaction_root(&info.executed_transactions);

    let (excess_blob_gas, blob_gas_used) = ctx.blob_fields(info);
    let extra_data = ctx.extra_data()?;

    let header = Header {
        parent_hash: ctx.parent().hash(),
        ommers_hash: EMPTY_OMMER_ROOT_HASH,
        beneficiary: ctx.evm_env.block_env.beneficiary,
        state_root,
        transactions_root,
        receipts_root,
        withdrawals_root,
        logs_bloom,
        timestamp: ctx.attributes().payload_attributes.timestamp,
        mix_hash: ctx.attributes().payload_attributes.prev_randao,
        nonce: BEACON_NONCE.into(),
        base_fee_per_gas: Some(ctx.base_fee()),
        number: ctx.parent().number + 1,
        gas_limit: ctx.block_gas_limit(),
        difficulty: U256::ZERO,
        gas_used: info.cumulative_gas_used,
        extra_data,
        parent_beacon_block_root: ctx.attributes().payload_attributes.parent_beacon_block_root,
        blob_gas_used,
        excess_blob_gas,
        requests_hash,
    };

    // seal the block
    let block = alloy_consensus::Block::<OpTransactionSigned>::new(
        header,
        BlockBody {
            transactions: info.executed_transactions.clone(),
            ommers: vec![],
            withdrawals: ctx.withdrawals().cloned(),
        },
    );

    let recovered_block =
        RecoveredBlock::new_unhashed(block.clone(), info.executed_senders.clone());
    // create the executed block data

    let executed = BuiltPayloadExecutedBlock {
        recovered_block: Arc::new(recovered_block),
        execution_output: Arc::new(BlockExecutionOutput {
            result: BlockExecutionResult {
                receipts: info.receipts.clone(),
                requests: vec![].into(),
                gas_used: info.cumulative_gas_used,
                blob_gas_used: 0,
            },
            state: state.take_bundle(),
        }),
        hashed_state: Either::Left(Arc::new(hashed_state)),
        trie_updates: Either::Left(Arc::new(trie_output)),
    };
    debug!(target: "payload_builder", message = "Executed block created");

    let sealed_block = Arc::new(block.seal_slow());
    debug!(target: "payload_builder", ?sealed_block, "sealed built block");

    let block_hash = sealed_block.hash();

    // pick the new transactions from the info field and update the last flashblock index
    let new_transactions = info.executed_transactions[info.extra.last_flashblock_index..].to_vec();

    let new_transactions_encoded =
        new_transactions.into_iter().map(|tx| tx.encoded_2718().into()).collect::<Vec<_>>();

    let min_tx_index = info.extra.last_flashblock_index as u64;
    let max_tx_index = min_tx_index + new_transactions_encoded.len() as u64;

    info.extra.last_flashblock_index = info.executed_transactions.len();

    // finalize and build the FAL
    let fal_builder = std::mem::take(&mut info.extra.access_list_builder);
    let access_list = fal_builder.build(min_tx_index, max_tx_index);

    let metadata: FlashblocksMetadata =
        FlashblocksMetadata { block_number: ctx.parent().number + 1, access_list };

    let (_, blob_gas_used) = ctx.blob_fields(info);

    // Prepare the flashblocks message
    let fb_payload = FlashblocksPayloadV1 {
        payload_id: ctx.payload_id(),
        index: 0,
        base: Some(ExecutionPayloadBaseV1 {
            parent_beacon_block_root: ctx
                .attributes()
                .payload_attributes
                .parent_beacon_block_root
                .unwrap(),
            parent_hash: ctx.parent().hash(),
            fee_recipient: ctx.attributes().suggested_fee_recipient(),
            prev_randao: ctx.attributes().payload_attributes.prev_randao,
            block_number: ctx.parent().number + 1,
            gas_limit: ctx.block_gas_limit(),
            timestamp: ctx.attributes().payload_attributes.timestamp,
            extra_data: ctx.extra_data()?,
            base_fee_per_gas: ctx.base_fee().try_into().unwrap(),
        }),
        diff: ExecutionPayloadFlashblockDeltaV1 {
            state_root,
            receipts_root,
            logs_bloom,
            gas_used: info.cumulative_gas_used,
            block_hash,
            transactions: new_transactions_encoded,
            withdrawals: ctx.withdrawals().cloned().unwrap_or_default().to_vec(),
            withdrawals_root: withdrawals_root.unwrap_or_default(),
            blob_gas_used,
        },
        metadata: serde_json::to_value(&metadata).unwrap_or_default(),
    };

    // We clean bundle and place initial state transaction back
    state.take_bundle();
    state.transition_state = untouched_transition_state;

    Ok((
        OpBuiltPayload::new(ctx.payload_id(), sealed_block, info.total_fees, Some(executed)),
        fb_payload,
    ))
}

/// Build a block with a pre-computed state root from the streaming state root task.
///
/// This is similar to `build_block` but skips the state root calculation since it's
/// already been computed by the streaming task.
#[allow(dead_code)]
pub(super) fn build_block_with_state_root<DB, P>(
    state: &mut State<DB>,
    ctx: &OpPayloadBuilderCtx,
    info: &mut ExecutionInfo,
    state_root: B256,
    trie_output: TrieUpdates,
    hashed_state: HashedPostState,
) -> Result<(OpBuiltPayload, FlashblocksPayloadV1), PayloadBuilderError>
where
    DB: Database<Error = ProviderError> + AsRef<P> + revm::Database,
    P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
{
    // We use it to preserve state, so we run merge_transitions on transition state at most once
    let untouched_transition_state = state.transition_state.clone();
    let state_merge_start_time = Instant::now();
    state.merge_transitions(BundleRetention::Reverts);
    let state_transition_merge_time = state_merge_start_time.elapsed();
    ctx.metrics.state_transition_merge_duration.record(state_transition_merge_time);
    ctx.metrics.state_transition_merge_gauge.set(state_transition_merge_time);

    let block_number = ctx.block_number();
    assert_eq!(block_number, ctx.parent().number + 1);

    let execution_outcome = ExecutionOutcome::new(
        state.bundle_state.clone(),
        vec![info.receipts.clone()],
        block_number,
        vec![],
    );

    let receipts_root = execution_outcome
        .generic_receipts_root_slow(block_number, |receipts| {
            calculate_receipt_root_no_memo_optimism(
                receipts,
                &ctx.chain_spec,
                ctx.attributes().timestamp(),
            )
        })
        .expect("Number is in range");
    let logs_bloom = execution_outcome.block_logs_bloom(block_number).expect("Number is in range");

    // State root is already computed by the streaming task
    debug!(
        target: "payload_builder",
        ?state_root,
        "Using pre-computed state root from streaming task"
    );

    let mut requests_hash = None;
    let withdrawals_root =
        if ctx.chain_spec.is_isthmus_active_at_timestamp(ctx.attributes().timestamp()) {
            requests_hash = Some(EMPTY_REQUESTS_HASH);
            Some(
                isthmus::withdrawals_root(execution_outcome.state(), state.database.as_ref())
                    .map_err(PayloadBuilderError::other)?,
            )
        } else if ctx.chain_spec.is_canyon_active_at_timestamp(ctx.attributes().timestamp()) {
            Some(EMPTY_WITHDRAWALS)
        } else {
            None
        };

    // create the block header
    let transactions_root = proofs::calculate_transaction_root(&info.executed_transactions);

    let (excess_blob_gas, blob_gas_used) = ctx.blob_fields(info);
    let extra_data = ctx.extra_data()?;

    let header = Header {
        parent_hash: ctx.parent().hash(),
        ommers_hash: EMPTY_OMMER_ROOT_HASH,
        beneficiary: ctx.evm_env.block_env.beneficiary,
        state_root,
        transactions_root,
        receipts_root,
        withdrawals_root,
        logs_bloom,
        timestamp: ctx.attributes().payload_attributes.timestamp,
        mix_hash: ctx.attributes().payload_attributes.prev_randao,
        nonce: BEACON_NONCE.into(),
        base_fee_per_gas: Some(ctx.base_fee()),
        number: ctx.parent().number + 1,
        gas_limit: ctx.block_gas_limit(),
        difficulty: U256::ZERO,
        gas_used: info.cumulative_gas_used,
        extra_data,
        parent_beacon_block_root: ctx.attributes().payload_attributes.parent_beacon_block_root,
        blob_gas_used,
        excess_blob_gas,
        requests_hash,
    };

    // seal the block
    let block = alloy_consensus::Block::<OpTransactionSigned>::new(
        header,
        BlockBody {
            transactions: info.executed_transactions.clone(),
            ommers: vec![],
            withdrawals: ctx.withdrawals().cloned(),
        },
    );

    let recovered_block =
        RecoveredBlock::new_unhashed(block.clone(), info.executed_senders.clone());

    let executed = BuiltPayloadExecutedBlock {
        recovered_block: Arc::new(recovered_block),
        execution_output: Arc::new(BlockExecutionOutput {
            result: BlockExecutionResult {
                receipts: info.receipts.clone(),
                requests: vec![].into(),
                gas_used: info.cumulative_gas_used,
                blob_gas_used: 0,
            },
            state: state.take_bundle(),
        }),
        hashed_state: Either::Left(Arc::new(hashed_state)),
        trie_updates: Either::Left(Arc::new(trie_output)),
    };
    debug!(target: "payload_builder", message = "Executed block created with streaming state root");

    let sealed_block = Arc::new(block.seal_slow());
    debug!(target: "payload_builder", ?sealed_block, "sealed built block");

    let block_hash = sealed_block.hash();

    // pick the new transactions from the info field and update the last flashblock index
    let new_transactions = info.executed_transactions[info.extra.last_flashblock_index..].to_vec();

    let new_transactions_encoded =
        new_transactions.into_iter().map(|tx| tx.encoded_2718().into()).collect::<Vec<_>>();

    let min_tx_index = info.extra.last_flashblock_index as u64;
    let max_tx_index = min_tx_index + new_transactions_encoded.len() as u64;

    info.extra.last_flashblock_index = info.executed_transactions.len();

    // finalize and build the FAL
    let fal_builder = std::mem::take(&mut info.extra.access_list_builder);
    let access_list = fal_builder.build(min_tx_index, max_tx_index);

    let metadata: FlashblocksMetadata =
        FlashblocksMetadata { block_number: ctx.parent().number + 1, access_list };

    let (_, blob_gas_used) = ctx.blob_fields(info);

    // Prepare the flashblocks message
    let fb_payload = FlashblocksPayloadV1 {
        payload_id: ctx.payload_id(),
        index: 0,
        base: Some(ExecutionPayloadBaseV1 {
            parent_beacon_block_root: ctx
                .attributes()
                .payload_attributes
                .parent_beacon_block_root
                .unwrap(),
            parent_hash: ctx.parent().hash(),
            fee_recipient: ctx.attributes().suggested_fee_recipient(),
            prev_randao: ctx.attributes().payload_attributes.prev_randao,
            block_number: ctx.parent().number + 1,
            gas_limit: ctx.block_gas_limit(),
            timestamp: ctx.attributes().payload_attributes.timestamp,
            extra_data: ctx.extra_data()?,
            base_fee_per_gas: ctx.base_fee().try_into().unwrap(),
        }),
        diff: ExecutionPayloadFlashblockDeltaV1 {
            state_root,
            receipts_root,
            logs_bloom,
            gas_used: info.cumulative_gas_used,
            block_hash,
            transactions: new_transactions_encoded,
            withdrawals: ctx.withdrawals().cloned().unwrap_or_default().to_vec(),
            withdrawals_root: withdrawals_root.unwrap_or_default(),
            blob_gas_used,
        },
        metadata: serde_json::to_value(&metadata).unwrap_or_default(),
    };

    // We clean bundle and place initial state transaction back
    state.take_bundle();
    state.transition_state = untouched_transition_state;

    Ok((
        OpBuiltPayload::new(ctx.payload_id(), sealed_block, info.total_fees, Some(executed)),
        fb_payload,
    ))
}
