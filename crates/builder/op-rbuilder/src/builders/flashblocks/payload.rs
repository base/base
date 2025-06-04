use core::time::Duration;
use std::{sync::Arc, time::Instant};

use super::{config::FlashblocksConfig, wspub::WebSocketPublisher};
use crate::{
    builders::{
        context::{estimate_gas_for_builder_tx, OpPayloadBuilderCtx},
        flashblocks::config::FlashBlocksConfigExt,
        generator::{BlockCell, BuildArguments},
        BuilderConfig,
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
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

#[derive(Debug, Default)]
struct ExtraExecutionInfo {
    /// Index of the last consumed flashblock
    pub last_flashblock_index: usize,
}

/// Optimism's payload builder
#[derive(Debug, Clone)]
pub struct OpPayloadBuilder<Pool, Client> {
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
}

impl<Pool, Client> OpPayloadBuilder<Pool, Client> {
    /// `OpPayloadBuilder` constructor.
    pub fn new(
        evm_config: OpEvmConfig,
        pool: Pool,
        client: Client,
        config: BuilderConfig<FlashblocksConfig>,
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
        })
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
        let BuildArguments { config, cancel, .. } = args;

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

        let ctx = OpPayloadBuilderCtx {
            evm_config: self.evm_config.clone(),
            chain_spec: self.client.chain_spec(),
            config,
            evm_env,
            block_env_attributes,
            cancel,
            da_config: self.config.da_config.clone(),
            builder_signer: self.config.builder_signer,
            metrics: Default::default(),
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
        // TODO: we could optimise this and subtract this only for the last flashblocks
        let message = format!("Block Number: {}", ctx.block_number()).into_bytes();
        let builder_tx_gas = ctx
            .builder_signer()
            .map_or(0, |_| estimate_gas_for_builder_tx(message.clone()));
        let builder_tx_da_size = ctx
            .estimate_builder_tx_da_size(&mut db, builder_tx_gas, message.clone())
            .unwrap_or(0);

        let mut info = execute_pre_steps(&mut db, &ctx)?;
        ctx.metrics
            .sequencer_tx_duration
            .record(sequencer_tx_start_time.elapsed());

        let (payload, fb_payload, mut bundle_state) = build_block(db, &ctx, &mut info)?;

        best_payload.set(payload.clone());
        self.ws_pub
            .publish(&fb_payload)
            .map_err(PayloadBuilderError::other)?;

        tracing::info!(target: "payload_builder", "Fallback block built");
        ctx.metrics
            .payload_num_tx
            .record(info.executed_transactions.len() as f64);

        if ctx.attributes().no_tx_pool {
            tracing::info!(
                target: "payload_builder",
                "No transaction pool, skipping transaction pool processing",
            );

            self.metrics
                .total_block_built_duration
                .record(block_build_start_time.elapsed());

            // return early since we don't need to build a block with transactions from the pool
            return Ok(());
        }
        let gas_per_batch = ctx.block_gas_limit() / self.config.flashblocks_per_block();
        let mut total_gas_per_batch = gas_per_batch;
        let da_per_batch = ctx
            .da_config
            .max_da_block_size()
            .map(|da_limit| da_limit / self.config.flashblocks_per_block());
        // Check that builder tx won't affect fb limit too much
        if let Some(da_limit) = da_per_batch {
            // We error if we can't insert any tx aside from builder tx in flashblock
            if da_limit / 2 < builder_tx_da_size {
                error!("Builder tx da size subtraction caused max_da_block_size to be 0. No transaction would be included.");
            }
        }
        let mut total_da_per_batch = da_per_batch;

        let last_flashblock = self.config.flashblocks_per_block().saturating_sub(1);
        let mut flashblock_count = 0;
        // Create a channel to coordinate flashblock building
        let (build_tx, mut build_rx) = mpsc::channel(1);

        // Spawn the timer task that signals when to build a new flashblock
        let cancel_clone = ctx.cancel.clone();
        let interval = self.config.specific.interval;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(interval);
            loop {
                tokio::select! {
                // Add a cancellation check that only runs every 10ms to avoid tight polling
                _ = tokio::time::sleep(Duration::from_millis(10)) => {
                    if cancel_clone.is_cancelled() {
                            tracing::info!(target: "payload_builder", "Job cancelled during sleep, stopping payload building");
                            drop(build_tx);
                            break;
                        }
                    }
                _ = interval.tick() => {
                            if let Err(err) = build_tx.send(()).await {
                                error!(target: "payload_builder", "Error sending build signal: {}", err);
                                break;
                            }
                        }
                }
            }
        });

        // Process flashblocks in a blocking loop
        loop {
            // Block on receiving a message, break on cancellation or closed channel
            let received = tokio::task::block_in_place(|| {
                // Get runtime handle
                let rt = tokio::runtime::Handle::current();

                // Run the async operation to completion, blocking the current thread
                rt.block_on(async {
                    // Check for cancellation first
                    if ctx.cancel.is_cancelled() {
                        tracing::info!(
                            target: "payload_builder",
                            "Job cancelled, stopping payload building",
                        );
                        return None;
                    }

                    // Wait for next message
                    build_rx.recv().await
                })
            });

            // Exit loop if channel closed or cancelled
            match received {
                Some(()) => {
                    if flashblock_count >= self.config.flashblocks_per_block() {
                        tracing::info!(
                            target: "payload_builder",
                            target = self.config.flashblocks_per_block(),
                            flashblock_count = flashblock_count,
                            block_number = ctx.block_number(),
                            "Skipping flashblock reached target",
                        );
                        continue;
                    }

                    // Continue with flashblock building
                    tracing::info!(
                        target: "payload_builder",
                        block_number = ctx.block_number(),
                        flashblock_count = flashblock_count,
                        target_gas = total_gas_per_batch,
                        gas_used = info.cumulative_gas_used,
                        target_da = total_da_per_batch.unwrap_or(0),
                        da_used = info.cumulative_da_bytes_used,
                        "Building flashblock",
                    );
                    let flashblock_build_start_time = Instant::now();
                    let state = StateProviderDatabase::new(&state_provider);
                    invoke_on_last_flashblock(flashblock_count, last_flashblock, || {
                        total_gas_per_batch -= builder_tx_gas;
                        // saturating sub just in case, we will log an error if da_limit too small for builder_tx_da_size
                        if let Some(da_limit) = total_da_per_batch.as_mut() {
                            *da_limit = da_limit.saturating_sub(builder_tx_da_size);
                        }
                    });
                    let mut db = State::builder()
                        .with_database(state)
                        .with_bundle_update()
                        .with_bundle_prestate(bundle_state)
                        .build();

                    let best_txs_start_time = Instant::now();
                    let best_txs = BestPayloadTransactions::new(
                        self.pool
                            .best_transactions_with_attributes(ctx.best_transaction_attributes()),
                    );
                    ctx.metrics
                        .transaction_pool_fetch_duration
                        .record(best_txs_start_time.elapsed());

                    let tx_execution_start_time = Instant::now();
                    ctx.execute_best_transactions(
                        &mut info,
                        &mut db,
                        best_txs,
                        total_gas_per_batch.min(ctx.block_gas_limit()),
                        total_da_per_batch,
                    )?;
                    ctx.metrics
                        .payload_tx_simulation_duration
                        .record(tx_execution_start_time.elapsed());

                    if ctx.cancel.is_cancelled() {
                        tracing::info!(
                            target: "payload_builder",
                            "Job cancelled, stopping payload building",
                        );
                        // if the job was cancelled, stop
                        return Ok(());
                    }

                    // If it is the last flashblocks, add the builder txn to the block if enabled
                    invoke_on_last_flashblock(flashblock_count, last_flashblock, || {
                        ctx.add_builder_tx(&mut info, &mut db, builder_tx_gas, message.clone());
                    });

                    let total_block_built_duration = Instant::now();
                    let build_result = build_block(db, &ctx, &mut info);
                    ctx.metrics
                        .total_block_built_duration
                        .record(total_block_built_duration.elapsed());

                    // Handle build errors with match pattern
                    match build_result {
                        Err(err) => {
                            // Track invalid/bad block
                            self.metrics.invalid_blocks_count.increment(1);
                            error!(target: "payload_builder", "Failed to build block {}, flashblock {}: {}", ctx.block_number(), flashblock_count, err);
                            // Return the error
                            return Err(err);
                        }
                        Ok((new_payload, mut fb_payload, new_bundle_state)) => {
                            fb_payload.index = flashblock_count + 1; // we do this because the fallback block is index 0
                            fb_payload.base = None;

                            self.ws_pub
                                .publish(&fb_payload)
                                .map_err(PayloadBuilderError::other)?;

                            // Record flashblock build duration
                            self.metrics
                                .flashblock_build_duration
                                .record(flashblock_build_start_time.elapsed());
                            ctx.metrics
                                .payload_byte_size
                                .record(new_payload.block().size() as f64);
                            ctx.metrics
                                .payload_num_tx
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
                            flashblock_count += 1;
                            tracing::info!(target: "payload_builder", "Flashblock {} built", flashblock_count);
                        }
                    }
                }
                None => {
                    // Exit loop if channel closed or cancelled
                    self.metrics.block_built_success.increment(1);
                    self.metrics
                        .flashblock_count
                        .record(flashblock_count as f64);
                    return Ok(());
                }
            }
        }
    }
}

impl<Pool, Client> crate::builders::generator::PayloadBuilder for OpPayloadBuilder<Pool, Client>
where
    Pool: PoolBounds,
    Client: ClientBounds,
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

fn execute_pre_steps<DB>(
    state: &mut State<DB>,
    ctx: &OpPayloadBuilderCtx,
) -> Result<ExecutionInfo<ExtraExecutionInfo>, PayloadBuilderError>
where
    DB: Database<Error = ProviderError>,
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

fn build_block<DB, P>(
    mut state: State<DB>,
    ctx: &OpPayloadBuilderCtx,
    info: &mut ExecutionInfo<ExtraExecutionInfo>,
) -> Result<(OpBuiltPayload, FlashblocksPayloadV1, BundleState), PayloadBuilderError>
where
    DB: Database<Error = ProviderError> + AsRef<P>,
    P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
{
    // TODO: We must run this only once per block, but we are running it on every flashblock
    // merge all transitions into bundle state, this would apply the withdrawal balance changes
    // and 4788 contract call
    let state_merge_start_time = Instant::now();
    state.merge_transitions(BundleRetention::Reverts);
    ctx.metrics
        .state_transition_merge_duration
        .record(state_merge_start_time.elapsed());

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
    ctx.metrics
        .state_root_calculation_duration
        .record(state_root_start_time.elapsed());

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

pub fn invoke_on_last_flashblock<F: FnOnce()>(
    current_flashblock: u64,
    flashblock_limit: u64,
    fun: F,
) {
    if current_flashblock == flashblock_limit {
        fun()
    }
}
