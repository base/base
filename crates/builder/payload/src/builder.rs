/// Optimism payload builder implementation for flashblocks.
use core::time::Duration;
use std::{sync::Arc, time::Instant};

use alloy_evm::Database;
use eyre::WrapErr as _;
use reth_basic_payload_builder::BuildOutcome;
use reth_evm::ConfigureEvm;
use reth_node_api::PayloadBuilderError;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_forks::OpHardforks;
use reth_optimism_node::{OpBuiltPayload, OpPayloadBuilderAttributes};
use reth_optimism_primitives::OpTransactionSigned;
use reth_payload_primitives::{BuiltPayload, PayloadBuilderAttributes};
use reth_payload_util::BestPayloadTransactions;
use reth_provider::{HashedPostStateProvider, ProviderError, StateRootProvider, StorageRootProvider};
use reth_revm::{State, database::StateProviderDatabase};
use reth_transaction_pool::TransactionPool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, metadata::Level, span};

use crate::{
    BlockCell, BuildArguments, ExecutionInfo, FlashblockPublisher, FlashblockScheduler,
    FlashblocksExtraCtx, PayloadBuilderConfig,
    assembler::BlockAssembler,
    best_txs::BestFlashblocksTxs,
    bounds::{ClientBounds, PoolBounds},
    context::OpPayloadBuilderCtx,
    executor::TxExecutor,
    metrics::BuilderMetrics,
    publisher::{GuardedPublisher, PublishResult},
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

/// Optimism's payload builder
#[derive(Clone)]
pub struct OpPayloadBuilder<Pool, Client> {
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
    pub ws_pub: Arc<dyn FlashblockPublisher>,
    /// System configuration for the builder
    pub config: PayloadBuilderConfig,
    /// The metrics for the builder
    pub metrics: Arc<BuilderMetrics>,
}

impl<Pool: std::fmt::Debug, Client: std::fmt::Debug> std::fmt::Debug
    for OpPayloadBuilder<Pool, Client>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpPayloadBuilder")
            .field("evm_config", &self.evm_config)
            .field("pool", &self.pool)
            .field("client", &self.client)
            .field("payload_tx", &self.payload_tx)
            .field("ws_pub", &"<dyn FlashblockPublisher>")
            .field("config", &self.config)
            .field("metrics", &self.metrics)
            .finish()
    }
}

impl<Pool, Client> OpPayloadBuilder<Pool, Client> {
    /// `OpPayloadBuilder` constructor.
    pub fn new(
        evm_config: OpEvmConfig,
        pool: Pool,
        client: Client,
        config: PayloadBuilderConfig,
        payload_tx: mpsc::Sender<OpBuiltPayload>,
        ws_pub: Arc<dyn FlashblockPublisher>,
        metrics: Arc<BuilderMetrics>,
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
            max_execution_time_per_tx_us: self.config.max_execution_time_per_tx_us,
            max_state_root_time_per_tx_us: self.config.max_state_root_time_per_tx_us,
            flashblock_execution_time_budget_us: self.config.flashblock_execution_time_budget_us,
            block_state_root_time_budget_us: self.config.block_state_root_time_budget_us,
            execution_metering_mode: self.config.execution_metering_mode,
            metering_provider: Arc::clone(&self.config.metering_provider),
        })
    }

    /// Constructs an Optimism payload from the transactions sent via the
    /// Payload attributes by the sequencer. If the `no_tx_pool` argument is passed in
    /// the payload attributes, the transaction pool will be ignored and the only transactions
    /// included in the payload will be those sent through the attributes.
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

        let sequencer_tx_start_time = Instant::now();
        let mut state =
            State::builder().with_database(cached_reads.as_db_mut(db)).with_bundle_update().build();

        let mut info = TxExecutor::new(&ctx)
            .execute_pre_steps(&mut state)
            .map_err(|e| PayloadBuilderError::Other(e.into()))?;

        let sequencer_tx_time = sequencer_tx_start_time.elapsed();
        ctx.metrics.sequencer_tx_duration.record(sequencer_tx_time);
        ctx.metrics.sequencer_tx_gauge.set(sequencer_tx_time);

        let calculate_state_root =
            !disable_state_root || ctx.attributes().no_tx_pool;
        let (payload, fb_payload) =
            BlockAssembler::new(&ctx, &mut info, calculate_state_root).assemble(&mut state)?;

        self.payload_tx.send(payload.clone()).await.map_err(PayloadBuilderError::other)?;
        best_payload.set(payload);

        info!(
            target: "payload_builder",
            message = "Fallback block built",
            payload_id = fb_payload.payload_id.to_string(),
        );

        if !ctx.attributes().no_tx_pool {
            let flashblock_byte_size = self
                .ws_pub
                .publish(&fb_payload)
                .map_err(|e| PayloadBuilderError::Other(e.into()))?;
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

            return Ok(());
        }

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

        let batch_limits = crate::FlashblockLimits::for_batch(
            ctx.block_gas_limit(),
            ctx.da_config.max_da_block_size(),
            info.da_footprint_scalar,
            ctx.flashblock_execution_time_budget_us,
            ctx.block_state_root_time_budget_us,
            flashblocks_per_block,
        );

        let extra = FlashblocksExtraCtx {
            flashblock_index: 1,
            target_flashblock_count: flashblocks_per_block,
            target_gas_for_batch: batch_limits.gas_per_batch,
            target_da_for_batch: batch_limits.da_per_batch,
            target_da_footprint_for_batch: batch_limits.da_footprint_per_batch,
            target_execution_time_for_batch_us: batch_limits.execution_time_per_batch_us,
            target_state_root_time_for_batch_us: batch_limits.state_root_time_per_batch_us,
            gas_per_batch: batch_limits.gas_per_batch,
            da_per_batch: batch_limits.da_per_batch,
            da_footprint_per_batch: batch_limits.da_footprint_per_batch,
            execution_time_per_batch_us: batch_limits.execution_time_per_batch_us,
            state_root_time_per_batch_us: batch_limits.state_root_time_per_batch_us,
            disable_state_root,
        };

        let mut fb_cancel = block_cancel.child_token();
        let mut ctx = self
            .get_op_payload_builder_ctx(config, fb_cancel.clone(), extra)
            .map_err(|e| PayloadBuilderError::Other(e.into()))?;

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
                            fb_cancel.cancel();
                            fb_cancel = block_cancel.child_token();
                            if tx.send(fb_cancel.clone()).await.is_err() {
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

        let guarded_pub = GuardedPublisher::new(Arc::clone(&self.ws_pub), publish_guard);

        loop {
            let fb_span = if span.is_none() {
                tracing::Span::none()
            } else {
                span!(parent: &span, Level::INFO, "build_flashblock")
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

            let next_flashblocks_ctx = match self
                .build_next_flashblock(
                    &ctx,
                    &mut info,
                    &mut state,
                    &mut best_txs,
                    &block_cancel,
                    &best_payload,
                    &guarded_pub,
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
        guarded_pub: &GuardedPublisher,
        span: &tracing::Span,
    ) -> eyre::Result<Option<FlashblocksExtraCtx>> {
        let flashblock_index = ctx.flashblock_index();

        info!(
            target: "payload_builder",
            block_number = ctx.block_number(),
            flashblock_index,
            target_gas = ctx.extra.target_gas_for_batch,
            gas_used = info.cumulative_gas_used,
            target_da = ctx.extra.target_da_for_batch,
            da_used = info.cumulative_da_bytes_used,
            block_gas_used = ctx.block_gas_limit(),
            target_da_footprint = ctx.extra.target_da_footprint_for_batch,
            flashblock_execution_time_limit_us = ?ctx.extra.execution_time_per_batch_us,
            target_state_root_time_for_batch_us = ?ctx.extra.target_state_root_time_for_batch_us,
            "Building flashblock",
        );
        let flashblock_build_start_time = Instant::now();

        info.reset_flashblock_execution_time();

        let best_txs_start_time = Instant::now();
        best_txs.refresh_iterator(BestPayloadTransactions::new(
            self.pool.best_transactions_with_attributes(ctx.best_transaction_attributes()),
        ));
        let transaction_pool_fetch_time = best_txs_start_time.elapsed();
        ctx.metrics.transaction_pool_fetch_duration.record(transaction_pool_fetch_time);
        ctx.metrics.transaction_pool_fetch_gauge.set(transaction_pool_fetch_time);

        let tx_execution_start_time = Instant::now();
        TxExecutor::new(ctx)
            .execute_best_transactions(info, state, best_txs)
            .wrap_err("failed to execute best transactions")?;

        let new_transactions = info.executed_transactions[info.extra.last_flashblock_index..]
            .to_vec()
            .iter()
            .map(|tx| tx.tx_hash())
            .collect::<Vec<_>>();
        best_txs.mark_committed(&new_transactions);

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
        let calculate_state_root =
            !ctx.extra.disable_state_root || ctx.attributes().no_tx_pool;
        let build_result = BlockAssembler::new(ctx, info, calculate_state_root).assemble(state);
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

                match guarded_pub.publish_if_not_cancelled(&fb_payload, block_cancel)? {
                    PublishResult::Cancelled => {
                        self.record_flashblocks_metrics(
                            ctx,
                            info,
                            ctx.target_flashblock_count(),
                            span,
                            "Payload building complete, channel closed or job cancelled",
                        );
                        return Ok(None);
                    }
                    PublishResult::Published(flashblock_byte_size) => {
                        self.payload_tx
                            .send(new_payload.clone())
                            .await
                            .wrap_err("failed to send built payload to handler")?;
                        best_payload.set(new_payload);

                        ctx.metrics
                            .flashblock_build_duration
                            .record(flashblock_build_start_time.elapsed());
                        ctx.metrics
                            .flashblock_byte_size_histogram
                            .record(flashblock_byte_size as f64);
                        ctx.metrics
                            .flashblock_num_tx_histogram
                            .record(info.executed_transactions.len() as f64);
                    }
                }

                info!(
                    target: "payload_builder",
                    message = "Flashblock built",
                    flashblock_index,
                    current_gas = info.cumulative_gas_used,
                    current_da = info.cumulative_da_bytes_used,
                    target_flashblocks = ctx.target_flashblock_count(),
                );

                Ok(Some(ctx.extra.advance()))
            }
        }
    }

    /// Do some logging and metric recording when we stop building flashblocks.
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

        if info.cumulative_state_root_time_us > 0 {
            ctx.metrics
                .block_predicted_state_root_time_us
                .record(info.cumulative_state_root_time_us as f64);
        }

        tracing::debug!(
            target: "payload_builder",
            message = message,
            flashblocks_per_block,
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

        let (final_payload, _) = BlockAssembler::new(ctx, info, true).assemble(state)?;

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

    /// Calculate number of flashblocks.
    /// If dynamic is enabled this function will take time drift into account.
    pub fn calculate_flashblocks(&self, timestamp: u64) -> (u64, Duration) {
        FlashblockScheduler::schedule(&self.config, timestamp)
    }
}

/// A trait for building payloads that encapsulate Ethereum transactions.
#[async_trait::async_trait]
pub trait PayloadBuilder: Send + Sync + Clone {
    /// The payload attributes type to accept for building.
    type Attributes: PayloadBuilderAttributes;
    /// The type of the built payload.
    type BuiltPayload: BuiltPayload;

    /// Tries to build a transaction payload using provided arguments.
    async fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
        best_payload: BlockCell<Self::BuiltPayload>,
    ) -> Result<(), PayloadBuilderError>;
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
