use super::{config::FlashblocksConfig, wspub::WebSocketPublisher};
use crate::{
    builders::{
        context::{estimate_gas_for_builder_tx, OpPayloadBuilderCtx},
        flashblocks::config::FlashBlocksConfigExt,
        generator::{BlockCell, BuildArguments},
        BuilderConfig, BuilderTx,
    },
    metrics::OpRBuilderMetrics,
    primitives::reth::ExecutionInfo,
    traits::{ClientBounds, PoolBounds},
};
use alloy_consensus::{
    constants::EMPTY_WITHDRAWALS, proofs, BlockBody, Header, EMPTY_OMMER_ROOT_HASH,
};
use alloy_eips::{eip7685::EMPTY_REQUESTS_HASH, merge::BEACON_NONCE, Encodable2718};
use alloy_primitives::{map::foldhash::HashMap, Address, B256, U256};
use core::time::Duration;
use reth::payload::PayloadBuilderAttributes;
use reth_basic_payload_builder::BuildOutcome;
use reth_evm::{execute::BlockBuilder, ConfigureEvm};
use reth_node_api::{Block, NodePrimitives, PayloadBuilderError};
use reth_optimism_consensus::{calculate_receipt_root_no_memo_optimism, isthmus};
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_forks::OpHardforks;
use reth_optimism_node::{OpBuiltPayload, OpPayloadBuilderAttributes};
use reth_optimism_primitives::{OpPrimitives, OpReceipt, OpTransactionSigned};
use reth_payload_util::BestPayloadTransactions;
use reth_provider::{
    ExecutionOutcome, HashedPostStateProvider, ProviderError, StateRootProvider,
    StorageRootProvider,
};
use reth_revm::{
    database::StateProviderDatabase,
    db::{states::bundle_state::BundleRetention, BundleState},
    State,
};
use revm::Database;
use rollup_boost::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, FlashblocksPayloadV1,
};
use serde::{Deserialize, Serialize};
use std::{
    ops::{Div, Rem},
    sync::Arc,
    time::Instant,
};
use tokio::sync::{
    mpsc,
    mpsc::{error::SendError, Sender},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, metadata::Level, span, warn};

#[derive(Debug, Default)]
struct ExtraExecutionInfo {
    /// Index of the last consumed flashblock
    pub last_flashblock_index: usize,
}

#[derive(Debug, Default)]
struct FlashblocksExtraCtx {
    /// Current flashblock index
    pub flashblock_index: u64,
    /// Target flashblock count
    pub target_flashblock_count: u64,
}

impl OpPayloadBuilderCtx<FlashblocksExtraCtx> {
    /// Returns the current flashblock index
    pub fn flashblock_index(&self) -> u64 {
        self.extra_ctx.flashblock_index
    }

    /// Returns the target flashblock count
    pub fn target_flashblock_count(&self) -> u64 {
        self.extra_ctx.target_flashblock_count
    }

    /// Increments the flashblock index
    pub fn increment_flashblock_index(&mut self) -> u64 {
        self.extra_ctx.flashblock_index += 1;
        self.extra_ctx.flashblock_index
    }

    /// Sets the target flashblock count
    pub fn set_target_flashblock_count(&mut self, target_flashblock_count: u64) -> u64 {
        self.extra_ctx.target_flashblock_count = target_flashblock_count;
        self.extra_ctx.target_flashblock_count
    }

    /// Returns if the flashblock is the last one
    pub fn is_last_flashblock(&self) -> bool {
        self.flashblock_index() == self.target_flashblock_count() - 1
    }
}

/// Optimism's payload builder
#[derive(Debug, Clone)]
pub struct OpPayloadBuilder<Pool, Client, BT> {
    /// The type responsible for creating the evm.
    pub evm_config: OpEvmConfig,
    /// The transaction pool
    pub pool: Pool,
    /// Node client
    pub client: Client,
    /// WebSocket publisher for broadcasting flashblocks
    /// to all connected subscribers.
    pub ws_pub: Arc<WebSocketPublisher>,
    /// System configuration for the builder
    pub config: BuilderConfig<FlashblocksConfig>,
    /// The metrics for the builder
    pub metrics: Arc<OpRBuilderMetrics>,
    /// The end of builder transaction type
    #[allow(dead_code)]
    pub builder_tx: BT,
}

impl<Pool, Client, BT> OpPayloadBuilder<Pool, Client, BT> {
    /// `OpPayloadBuilder` constructor.
    pub fn new(
        evm_config: OpEvmConfig,
        pool: Pool,
        client: Client,
        config: BuilderConfig<FlashblocksConfig>,
        builder_tx: BT,
    ) -> eyre::Result<Self> {
        let metrics = Arc::new(OpRBuilderMetrics::default());
        let ws_pub = WebSocketPublisher::new(config.specific.ws_addr, Arc::clone(&metrics))?.into();
        Ok(Self {
            evm_config,
            pool,
            client,
            ws_pub,
            config,
            metrics,
            builder_tx,
        })
    }
}

impl<Pool, Client, BT> reth_basic_payload_builder::PayloadBuilder
    for OpPayloadBuilder<Pool, Client, BT>
where
    Pool: Clone + Send + Sync,
    Client: Clone + Send + Sync,
    BT: Clone + Send + Sync,
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

impl<Pool, Client, BT> OpPayloadBuilder<Pool, Client, BT>
where
    Pool: PoolBounds,
    Client: ClientBounds,
    BT: BuilderTx,
{
    /// Constructs an Optimism payload from the transactions sent via the
    /// Payload attributes by the sequencer. If the `no_tx_pool` argument is passed in
    /// the payload attributes, the transaction pool will be ignored and the only transactions
    /// included in the payload will be those sent through the attributes.
    ///
    /// Given build arguments including an Optimism client, transaction pool,
    /// and configuration, this function creates a transaction payload. Returns
    /// a result indicating success with the payload or an error in case of failure.
    fn build_payload(
        &self,
        args: BuildArguments<OpPayloadBuilderAttributes<OpTransactionSigned>, OpBuiltPayload>,
        best_payload: BlockCell<OpBuiltPayload>,
    ) -> Result<(), PayloadBuilderError> {
        let block_build_start_time = Instant::now();
        let BuildArguments {
            config,
            cancel: block_cancel,
            ..
        } = args;

        // We log only every 100th block to reduce usage
        let span = if cfg!(feature = "telemetry")
            && config.parent_header.number % self.config.sampling_ratio == 0
        {
            span!(Level::INFO, "build_payload")
        } else {
            tracing::Span::none()
        };
        let _entered = span.enter();
        span.record(
            "payload_id",
            config.attributes.payload_attributes.id.to_string(),
        );

        let chain_spec = self.client.chain_spec();
        let timestamp = config.attributes.timestamp();
        let block_env_attributes = OpNextBlockEnvAttributes {
            timestamp,
            suggested_fee_recipient: config.attributes.suggested_fee_recipient(),
            prev_randao: config.attributes.prev_randao(),
            gas_limit: config
                .attributes
                .gas_limit
                .unwrap_or(config.parent_header.gas_limit),
            parent_beacon_block_root: config
                .attributes
                .payload_attributes
                .parent_beacon_block_root,
            extra_data: if chain_spec.is_holocene_active_at_timestamp(timestamp) {
                config
                    .attributes
                    .get_holocene_extra_data(chain_spec.base_fee_params_at_timestamp(timestamp))
                    .map_err(PayloadBuilderError::other)?
            } else {
                Default::default()
            },
        };

        let evm_env = self
            .evm_config
            .next_evm_env(&config.parent_header, &block_env_attributes)
            .map_err(PayloadBuilderError::other)?;

        let mut ctx = OpPayloadBuilderCtx::<FlashblocksExtraCtx> {
            evm_config: self.evm_config.clone(),
            chain_spec: self.client.chain_spec(),
            config,
            evm_env,
            block_env_attributes,
            // Here we use parent token because child token handing is only for proper flashblocks
            cancel: block_cancel.clone(),
            da_config: self.config.da_config.clone(),
            builder_signer: self.config.builder_signer,
            metrics: Default::default(),
            extra_ctx: FlashblocksExtraCtx {
                flashblock_index: 0,
                target_flashblock_count: self.config.flashblocks_per_block(),
            },
        };

        let state_provider = self.client.state_by_block_hash(ctx.parent().hash())?;
        let state = StateProviderDatabase::new(&state_provider);

        // 1. execute the pre steps and seal an early block with that
        let sequencer_tx_start_time = Instant::now();
        let mut db = State::builder()
            .with_database(state)
            .with_bundle_update()
            .build();

        // We subtract gas limit and da limit for builder transaction from the whole limit
        let message = format!("Block Number: {}", ctx.block_number()).into_bytes();
        let builder_tx_gas = ctx
            .builder_signer()
            .map_or(0, |_| estimate_gas_for_builder_tx(message.clone()));
        let builder_tx_da_size = ctx
            .estimate_builder_tx_da_size(&mut db, builder_tx_gas, message.clone())
            .unwrap_or(0);

        let mut info = execute_pre_steps(&mut db, &ctx)?;
        let sequencer_tx_time = sequencer_tx_start_time.elapsed();
        ctx.metrics.sequencer_tx_duration.record(sequencer_tx_time);
        ctx.metrics.sequencer_tx_gauge.set(sequencer_tx_time);

        // If we have payload with txpool we add first builder tx right after deposits
        if !ctx.attributes().no_tx_pool {
            ctx.add_builder_tx(&mut info, &mut db, builder_tx_gas, message.clone());
        }

        let (payload, fb_payload, mut bundle_state) = build_block(db, &ctx, &mut info)?;

        best_payload.set(payload.clone());
        self.ws_pub
            .publish(&fb_payload)
            .map_err(PayloadBuilderError::other)?;

        info!(
            target: "payload_builder",
            message = "Fallback block built",
            payload_id = fb_payload.payload_id.to_string(),
        );

        ctx.metrics
            .payload_num_tx
            .record(info.executed_transactions.len() as f64);
        ctx.metrics
            .payload_num_tx_gauge
            .set(info.executed_transactions.len() as f64);

        if ctx.attributes().no_tx_pool {
            info!(
                target: "payload_builder",
                "No transaction pool, skipping transaction pool processing",
            );

            let total_block_building_time = block_build_start_time.elapsed();
            ctx.metrics
                .total_block_built_duration
                .record(total_block_building_time);
            ctx.metrics
                .total_block_built_gauge
                .set(total_block_building_time);

            // return early since we don't need to build a block with transactions from the pool
            return Ok(());
        }
        // We adjust our flashblocks timings based on time_drift if dynamic adjustment enable
        let (flashblocks_per_block, first_flashblock_offset) =
            self.calculate_flashblocks(timestamp);
        ctx.set_target_flashblock_count(flashblocks_per_block);
        info!(
            target: "payload_builder",
            message = "Performed flashblocks timing derivation",
            flashblocks_per_block = ctx.target_flashblock_count(),
            first_flashblock_offset = first_flashblock_offset.as_millis(),
            flashblocks_interval = self.config.specific.interval.as_millis(),
        );
        ctx.metrics.reduced_flashblocks_number.record(
            self.config
                .flashblocks_per_block()
                .saturating_sub(ctx.target_flashblock_count()) as f64,
        );
        ctx.metrics
            .first_flashblock_time_offset
            .record(first_flashblock_offset.as_millis() as f64);
        let gas_per_batch = ctx.block_gas_limit() / ctx.target_flashblock_count();
        let mut total_gas_per_batch = gas_per_batch;
        let da_per_batch = ctx
            .da_config
            .max_da_block_size()
            .map(|da_limit| da_limit / ctx.target_flashblock_count());
        // Check that builder tx won't affect fb limit too much
        if let Some(da_limit) = da_per_batch {
            // We error if we can't insert any tx aside from builder tx in flashblock
            if da_limit / 2 < builder_tx_da_size {
                error!("Builder tx da size subtraction caused max_da_block_size to be 0. No transaction would be included.");
            }
        }
        let mut total_da_per_batch = da_per_batch;

        // Account for already included builder tx
        total_gas_per_batch = total_gas_per_batch.saturating_sub(builder_tx_gas);
        if let Some(da_limit) = total_da_per_batch.as_mut() {
            *da_limit = da_limit.saturating_sub(builder_tx_da_size);
        }

        // This channel coordinates flashblock building
        let (fb_cancel_token_rx, mut fb_cancel_token_tx) =
            mpsc::channel((self.config.flashblocks_per_block() + 1) as usize);
        self.spawn_timer_task(
            block_cancel.clone(),
            fb_cancel_token_rx,
            first_flashblock_offset,
        );
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

            // We get token from time loop. Token from this channel means that we need to start build flashblock
            // Cancellation of this token means that we need to stop building flashblock.
            // If channel return None it means that we built all flashblock or parent_token got cancelled
            let fb_cancel_token =
                tokio::task::block_in_place(|| fb_cancel_token_tx.blocking_recv()).flatten();

            match fb_cancel_token {
                Some(cancel_token) => {
                    // We use fb_cancel_token inside context so we could exit from
                    // execute_best_transaction without cancelling parent token
                    ctx.cancel = cancel_token;
                    // TODO: remove this
                    if ctx.flashblock_index() >= ctx.target_flashblock_count() {
                        info!(
                            target: "payload_builder",
                            target = ctx.target_flashblock_count(),
                            flashblock_count = ctx.flashblock_index(),
                            block_number = ctx.block_number(),
                            "Skipping flashblock reached target",
                        );
                        continue;
                    }
                    // Continue with flashblock building
                    info!(
                        target: "payload_builder",
                        block_number = ctx.block_number(),
                        flashblock_count = ctx.flashblock_index(),
                        target_gas = total_gas_per_batch,
                        gas_used = info.cumulative_gas_used,
                        target_da = total_da_per_batch.unwrap_or(0),
                        da_used = info.cumulative_da_bytes_used,
                        "Building flashblock",
                    );
                    let flashblock_build_start_time = Instant::now();
                    let state = StateProviderDatabase::new(&state_provider);
                    // If it is the last flashblock, we need to account for the builder tx
                    if ctx.is_last_flashblock() {
                        total_gas_per_batch = total_gas_per_batch.saturating_sub(builder_tx_gas);
                        // saturating sub just in case, we will log an error if da_limit too small for builder_tx_da_size
                        if let Some(da_limit) = total_da_per_batch.as_mut() {
                            *da_limit = da_limit.saturating_sub(builder_tx_da_size);
                        }
                    }
                    let mut db = State::builder()
                        .with_database(state)
                        .with_bundle_update()
                        .with_bundle_prestate(bundle_state)
                        .build();

                    let best_txs_start_time = Instant::now();
                    let best_txs = BestPayloadTransactions::new(
                        // We are not using without_updates in here, so arriving transaction could target the current block
                        self.pool
                            .best_transactions_with_attributes(ctx.best_transaction_attributes()),
                    );
                    let transaction_pool_fetch_time = best_txs_start_time.elapsed();
                    ctx.metrics
                        .transaction_pool_fetch_duration
                        .record(transaction_pool_fetch_time);
                    ctx.metrics
                        .transaction_pool_fetch_gauge
                        .set(transaction_pool_fetch_time);

                    let tx_execution_start_time = Instant::now();
                    ctx.execute_best_transactions(
                        &mut info,
                        &mut db,
                        best_txs,
                        total_gas_per_batch.min(ctx.block_gas_limit()),
                        total_da_per_batch,
                    )?;
                    // We got block cancelled, we won't need anything from the block at this point
                    // Caution: this assume that block cancel token only cancelled when new FCU is received
                    if block_cancel.is_cancelled() {
                        ctx.metrics.block_built_success.increment(1);
                        ctx.metrics
                            .flashblock_count
                            .record(ctx.flashblock_index() as f64);
                        debug!(
                            target: "payload_builder",
                            message = "Payload building complete, job cancelled during execution"
                        );
                        span.record("flashblock_count", ctx.flashblock_index());
                        return Ok(());
                    }

                    let payload_tx_simulation_time = tx_execution_start_time.elapsed();
                    ctx.metrics
                        .payload_tx_simulation_duration
                        .record(payload_tx_simulation_time);
                    ctx.metrics
                        .payload_tx_simulation_gauge
                        .set(payload_tx_simulation_time);

                    // If it is the last flashblocks, add the builder txn to the block if enabled
                    if ctx.is_last_flashblock() {
                        ctx.add_builder_tx(&mut info, &mut db, builder_tx_gas, message.clone());
                    };

                    let total_block_built_duration = Instant::now();
                    let build_result = build_block(db, &ctx, &mut info);
                    let total_block_built_duration = total_block_built_duration.elapsed();
                    ctx.metrics
                        .total_block_built_duration
                        .record(total_block_built_duration);
                    ctx.metrics
                        .total_block_built_gauge
                        .set(total_block_built_duration);

                    // Handle build errors with match pattern
                    match build_result {
                        Err(err) => {
                            // Track invalid/bad block
                            ctx.metrics.invalid_blocks_count.increment(1);
                            error!(target: "payload_builder", "Failed to build block {}, flashblock {}: {}", ctx.block_number(), ctx.flashblock_index(), err);
                            // Return the error
                            return Err(err);
                        }
                        Ok((new_payload, mut fb_payload, new_bundle_state)) => {
                            fb_payload.index = ctx.increment_flashblock_index(); // fallback block is index 0, so we need to increment here
                            fb_payload.base = None;

                            // We check that child_job got cancelled before sending flashblock.
                            // This will ensure consistent timing between flashblocks.
                            tokio::task::block_in_place(|| {
                                tokio::runtime::Handle::current()
                                    .block_on(async { ctx.cancel.cancelled().await });
                            });
                            self.ws_pub
                                .publish(&fb_payload)
                                .map_err(PayloadBuilderError::other)?;

                            // Record flashblock build duration
                            ctx.metrics
                                .flashblock_build_duration
                                .record(flashblock_build_start_time.elapsed());
                            ctx.metrics
                                .flashblock_byte_size_histogram
                                .record(new_payload.block().size() as f64);
                            ctx.metrics
                                .flashblock_num_tx_histogram
                                .record(info.executed_transactions.len() as f64);

                            best_payload.set(new_payload.clone());
                            // Update bundle_state for next iteration
                            bundle_state = new_bundle_state;
                            total_gas_per_batch += gas_per_batch;
                            if let Some(da_limit) = da_per_batch {
                                if let Some(da) = total_da_per_batch.as_mut() {
                                    *da += da_limit;
                                } else {
                                    error!("Builder end up in faulty invariant, if da_per_batch is set then total_da_per_batch must be set");
                                }
                            }

                            info!(
                                target: "payload_builder",
                                message = "Flashblock built",
                                flashblock_count = ctx.flashblock_index(),
                                current_gas = info.cumulative_gas_used,
                                current_da = info.cumulative_da_bytes_used,
                                target_flashblocks = flashblocks_per_block,
                            );
                        }
                    }
                }
                None => {
                    // Exit loop if channel closed or cancelled
                    ctx.metrics.block_built_success.increment(1);
                    ctx.metrics
                        .flashblock_count
                        .record(ctx.flashblock_index() as f64);
                    ctx.metrics
                        .missing_flashblocks_count
                        .record(flashblocks_per_block.saturating_sub(ctx.flashblock_index()) as f64);
                    debug!(
                        target: "payload_builder",
                        message = "Payload building complete, channel closed or job cancelled",
                        missing_falshblocks = flashblocks_per_block.saturating_sub(ctx.flashblock_index()),
                        reduced_flashblocks = self.config.flashblocks_per_block().saturating_sub(flashblocks_per_block),
                    );
                    span.record("flashblock_count", ctx.flashblock_index());
                    return Ok(());
                }
            }
        }
    }

    /// Spawn task that will send new flashblock level cancel token in steady intervals (first interval
    /// may vary if --flashblocks.dynamic enabled)
    pub fn spawn_timer_task(
        &self,
        block_cancel: CancellationToken,
        flashblock_cancel_token_rx: Sender<Option<CancellationToken>>,
        first_flashblock_offset: Duration,
    ) {
        let interval = self.config.specific.interval;
        tokio::spawn(async move {
            let cancelled: Option<Result<(), SendError<Option<CancellationToken>>>> = block_cancel
                .run_until_cancelled(async {
                    // Create first fb interval already started
                    let mut timer = tokio::time::interval(first_flashblock_offset);
                    timer.tick().await;
                    let child_token = block_cancel.child_token();
                    flashblock_cancel_token_rx
                        .send(Some(child_token.clone()))
                        .await?;
                    timer.tick().await;
                    // Time to build flashblock has ended so we cancel the token
                    child_token.cancel();
                    // We would start using regular intervals from here on
                    let mut timer = tokio::time::interval(interval);
                    timer.tick().await;
                    loop {
                        // Initiate fb job
                        let child_token = block_cancel.child_token();
                        debug!(target: "payload_builder", "Sending child cancel token to execution loop");
                        flashblock_cancel_token_rx
                            .send(Some(child_token.clone()))
                            .await?;
                        timer.tick().await;
                        debug!(target: "payload_builder", "Cancelling child token to complete flashblock");
                        // Cancel job once time is up
                        child_token.cancel();
                    }
                })
                .await;
            if let Some(Err(err)) = cancelled {
                error!(target: "payload_builder", "Timer task encountered error: {err}");
            } else {
                info!(target: "payload_builder", "Building job cancelled, stopping payload building");
            }
        });
    }

    /// Calculate number of flashblocks.
    /// If dynamic is enabled this function will take time drift into the account.
    pub fn calculate_flashblocks(&self, timestamp: u64) -> (u64, Duration) {
        if self.config.specific.fixed {
            return (
                self.config.flashblocks_per_block(),
                // We adjust first FB to ensure that we have at least some time to make all FB in time
                self.config.specific.interval - self.config.specific.leeway_time,
            );
        }
        // We use this system time to determine remining time to build a block
        // Things to consider:
        // FCU(a) - FCU with attributes
        // FCU(a) could arrive with `block_time - fb_time < delay`. In this case we could only produce 1 flashblock
        // FCU(a) could arrive with `delay < fb_time` - in this case we will shrink first flashblock
        // FCU(a) could arrive with `fb_time < delay < block_time - fb_time` - in this case we will issue less flashblocks
        let target_time = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(timestamp)
            - self.config.specific.leeway_time;
        let now = std::time::SystemTime::now();
        let Ok(time_drift) = target_time.duration_since(now) else {
            error!(
                target: "payload_builder",
                message = "FCU arrived too late or system clock are unsynced",
                ?target_time,
                ?now,
            );
            return (
                self.config.flashblocks_per_block(),
                self.config.specific.interval,
            );
        };
        self.metrics.flashblocks_time_drift.record(
            self.config
                .block_time
                .as_millis()
                .saturating_sub(time_drift.as_millis()) as f64,
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
        let interval = self.config.specific.interval.as_millis() as u64;
        let time_drift = time_drift.as_millis() as u64;
        let first_flashblock_offset = time_drift.rem(interval);
        if first_flashblock_offset == 0 {
            // We have perfect division, so we use interval as first fb offset
            (time_drift.div(interval), Duration::from_millis(interval))
        } else {
            // Non-perfect division, so we account for it.
            (
                time_drift.div(interval) + 1,
                Duration::from_millis(first_flashblock_offset),
            )
        }
    }
}

impl<Pool, Client, BT> crate::builders::generator::PayloadBuilder
    for OpPayloadBuilder<Pool, Client, BT>
where
    Pool: PoolBounds,
    Client: ClientBounds,
    BT: BuilderTx + Clone + Send + Sync,
{
    type Attributes = OpPayloadBuilderAttributes<OpTransactionSigned>;
    type BuiltPayload = OpBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
        best_payload: BlockCell<Self::BuiltPayload>,
    ) -> Result<(), PayloadBuilderError> {
        self.build_payload(args, best_payload)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct FlashblocksMetadata {
    receipts: HashMap<B256, <OpPrimitives as NodePrimitives>::Receipt>,
    new_account_balances: HashMap<Address, U256>,
    block_number: u64,
}

fn execute_pre_steps<DB, ExtraCtx>(
    state: &mut State<DB>,
    ctx: &OpPayloadBuilderCtx<ExtraCtx>,
) -> Result<ExecutionInfo<ExtraExecutionInfo>, PayloadBuilderError>
where
    DB: Database<Error = ProviderError> + std::fmt::Debug,
    ExtraCtx: std::fmt::Debug + Default,
{
    // 1. apply pre-execution changes
    ctx.evm_config
        .builder_for_next_block(state, ctx.parent(), ctx.block_env_attributes.clone())
        .map_err(PayloadBuilderError::other)?
        .apply_pre_execution_changes()?;

    // 3. execute sequencer transactions
    let info = ctx.execute_sequencer_transactions(state)?;

    Ok(info)
}

fn build_block<DB, P, ExtraCtx>(
    mut state: State<DB>,
    ctx: &OpPayloadBuilderCtx<ExtraCtx>,
    info: &mut ExecutionInfo<ExtraExecutionInfo>,
) -> Result<(OpBuiltPayload, FlashblocksPayloadV1, BundleState), PayloadBuilderError>
where
    DB: Database<Error = ProviderError> + AsRef<P>,
    P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
    ExtraCtx: std::fmt::Debug + Default,
{
    // TODO: We must run this only once per block, but we are running it on every flashblock
    // merge all transitions into bundle state, this would apply the withdrawal balance changes
    // and 4788 contract call
    let state_merge_start_time = Instant::now();
    state.merge_transitions(BundleRetention::Reverts);
    let state_transition_merge_time = state_merge_start_time.elapsed();
    ctx.metrics
        .state_transition_merge_duration
        .record(state_transition_merge_time);
    ctx.metrics
        .state_transition_merge_gauge
        .set(state_transition_merge_time);

    let new_bundle = state.take_bundle();

    let block_number = ctx.block_number();
    assert_eq!(block_number, ctx.parent().number + 1);

    let execution_outcome = ExecutionOutcome::new(
        new_bundle.clone(),
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
    let logs_bloom = execution_outcome
        .block_logs_bloom(block_number)
        .expect("Number is in range");

    // // calculate the state root
    let state_root_start_time = Instant::now();
    let state_provider = state.database.as_ref();
    let hashed_state = state_provider.hashed_post_state(execution_outcome.state());
    let (state_root, _trie_output) = {
        state
            .database
            .as_ref()
            .state_root_with_updates(hashed_state.clone())
            .inspect_err(|err| {
                warn!(target: "payload_builder",
                parent_header=%ctx.parent().hash(),
                    %err,
                    "failed to calculate state root for payload"
                );
            })?
    };
    let state_root_calculation_time = state_root_start_time.elapsed();
    ctx.metrics
        .state_root_calculation_duration
        .record(state_root_calculation_time);
    ctx.metrics
        .state_root_calculation_gauge
        .set(state_root_calculation_time);

    let mut requests_hash = None;
    let withdrawals_root = if ctx
        .chain_spec
        .is_isthmus_active_at_timestamp(ctx.attributes().timestamp())
    {
        // always empty requests hash post isthmus
        requests_hash = Some(EMPTY_REQUESTS_HASH);

        // withdrawals root field in block header is used for storage root of L2 predeploy
        // `l2tol1-message-passer`
        Some(
            isthmus::withdrawals_root(execution_outcome.state(), state.database.as_ref())
                .map_err(PayloadBuilderError::other)?,
        )
    } else if ctx
        .chain_spec
        .is_canyon_active_at_timestamp(ctx.attributes().timestamp())
    {
        Some(EMPTY_WITHDRAWALS)
    } else {
        None
    };

    // create the block header
    let transactions_root = proofs::calculate_transaction_root(&info.executed_transactions);

    // OP doesn't support blobs/EIP-4844.
    // https://specs.optimism.io/protocol/exec-engine.html#ecotone-disable-blob-transactions
    // Need [Some] or [None] based on hardfork to match block hash.
    let (excess_blob_gas, blob_gas_used) = ctx.blob_fields();
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

    let sealed_block = Arc::new(block.seal_slow());
    debug!(target: "payload_builder", ?sealed_block, "sealed built block");

    let block_hash = sealed_block.hash();

    // pick the new transactions from the info field and update the last flashblock index
    let new_transactions = info.executed_transactions[info.extra.last_flashblock_index..].to_vec();

    let new_transactions_encoded = new_transactions
        .clone()
        .into_iter()
        .map(|tx| tx.encoded_2718().into())
        .collect::<Vec<_>>();

    let new_receipts = info.receipts[info.extra.last_flashblock_index..].to_vec();
    info.extra.last_flashblock_index = info.executed_transactions.len();
    let receipts_with_hash = new_transactions
        .iter()
        .zip(new_receipts.iter())
        .map(|(tx, receipt)| (tx.tx_hash(), receipt.clone()))
        .collect::<HashMap<B256, OpReceipt>>();
    let new_account_balances = new_bundle
        .state
        .iter()
        .filter_map(|(address, account)| account.info.as_ref().map(|info| (*address, info.balance)))
        .collect::<HashMap<Address, U256>>();

    let metadata: FlashblocksMetadata = FlashblocksMetadata {
        receipts: receipts_with_hash,
        new_account_balances,
        block_number: ctx.parent().number + 1,
    };

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
        },
        metadata: serde_json::to_value(&metadata).unwrap_or_default(),
    };

    Ok((
        OpBuiltPayload::new(
            ctx.payload_id(),
            sealed_block,
            info.total_fees,
            // This must be set to NONE for now because we are doing merge transitions on every flashblock
            // when it should only happen once per block, thus, it returns a confusing state back to op-reth.
            // We can live without this for now because Op syncs up the executed block using new_payload
            // calls, but eventually we would want to return the executed block here.
            None,
        ),
        fb_payload,
        new_bundle,
    ))
}
