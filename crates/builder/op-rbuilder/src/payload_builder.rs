use crate::{
    generator::{BlockCell, BlockPayloadJobGenerator, BuildArguments, PayloadBuilder},
    metrics::OpRBuilderMetrics,
    primitives::reth::ExecutionInfo,
    tx_signer::Signer,
};
use alloy_consensus::{
    constants::EMPTY_WITHDRAWALS, Eip658Value, Header, Transaction, Typed2718,
    EMPTY_OMMER_ROOT_HASH,
};
use alloy_eips::{eip7685::EMPTY_REQUESTS_HASH, merge::BEACON_NONCE, Encodable2718};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use alloy_primitives::{map::HashMap, Address, Bytes, B256, U256};
use alloy_rpc_types_engine::PayloadId;
use alloy_rpc_types_eth::Withdrawals;
use futures_util::{FutureExt, SinkExt};
use op_alloy_consensus::OpDepositReceipt;
use op_revm::OpSpecId;
use reth::{
    builder::{
        components::{PayloadBuilderBuilder, PayloadServiceBuilder},
        node::FullNodeTypes,
        BuilderContext,
    },
    payload::PayloadBuilderHandle,
};
use reth_basic_payload_builder::{BasicPayloadJobGeneratorConfig, BuildOutcome, PayloadConfig};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_evm::{
    env::EvmEnv, eth::receipt_builder::ReceiptBuilderCtx, execute::BlockBuilder, ConfigureEvm,
    Database, Evm, EvmError, InvalidTxError,
};
use reth_execution_types::ExecutionOutcome;
use reth_node_api::{NodePrimitives, NodeTypes, PrimitivesTy, TxTy};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_consensus::{calculate_receipt_root_no_memo_optimism, isthmus};
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_forks::OpHardforks;
use reth_optimism_node::OpEngineTypes;
use reth_optimism_payload_builder::{
    error::OpPayloadBuilderError,
    payload::{OpBuiltPayload, OpPayloadBuilderAttributes},
};
use reth_optimism_primitives::{OpPrimitives, OpReceipt, OpTransactionSigned};
use reth_optimism_txpool::OpPooledTx;
use reth_payload_builder::PayloadBuilderService;
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_payload_util::{BestPayloadTransactions, PayloadTransactions};
use reth_primitives::{BlockBody, SealedHeader};
use reth_primitives_traits::{proofs, Block as _, InMemorySize, SignedTransaction};
use reth_provider::{
    CanonStateSubscriptions, HashedPostStateProvider, ProviderError, StateProviderFactory,
    StateRootProvider, StorageRootProvider,
};
use reth_revm::database::StateProviderDatabase;
use reth_transaction_pool::{BestTransactionsAttributes, PoolTransaction, TransactionPool};
use revm::{
    context::{result::ResultAndState, Block as _},
    database::{states::bundle_state::BundleRetention, BundleState, State},
    DatabaseCommit,
};
use rollup_boost::primitives::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, FlashblocksPayloadV1,
};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_tungstenite::{accept_async, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, trace, warn};

/// Flashblocks specific payload building errors.
#[derive(Debug, thiserror::Error)]
pub enum FlashblockPayloadBuilderError {
    /// Thrown when the job was cancelled.
    #[error("error sending build signal")]
    SendBuildSignalError,
}

#[derive(Debug, Serialize, Deserialize)]
struct FlashblocksMetadata<N: NodePrimitives> {
    receipts: HashMap<B256, N::Receipt>,
    new_account_balances: HashMap<Address, U256>,
    block_number: u64,
}

#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct CustomOpPayloadBuilder {
    #[expect(dead_code)]
    builder_signer: Option<Signer>,
    flashblocks_ws_url: String,
    chain_block_time: u64,
    flashblock_block_time: u64,
    extra_block_deadline: std::time::Duration,
}

impl CustomOpPayloadBuilder {
    pub fn new(
        builder_signer: Option<Signer>,
        extra_block_deadline: std::time::Duration,
        flashblocks_ws_url: String,
        chain_block_time: u64,
        flashblock_block_time: u64,
    ) -> Self {
        Self {
            builder_signer,
            flashblocks_ws_url,
            chain_block_time,
            flashblock_block_time,
            extra_block_deadline,
        }
    }
}

impl<Node, Pool, Evm> PayloadBuilderBuilder<Node, Pool, Evm> for CustomOpPayloadBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            Payload = OpEngineTypes,
            ChainSpec = OpChainSpec,
            Primitives = OpPrimitives,
        >,
    >,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
    Evm: ConfigureEvm<
            Primitives = PrimitivesTy<Node::Types>,
            NextBlockEnvCtx = OpNextBlockEnvAttributes,
        > + 'static,
{
    type PayloadBuilder = OpPayloadBuilder<Pool, Node::Provider>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        _evm_config: Evm,
    ) -> eyre::Result<Self::PayloadBuilder> {
        Ok(OpPayloadBuilder::new(
            OpEvmConfig::optimism(ctx.chain_spec()),
            pool,
            ctx.provider().clone(),
            self.flashblocks_ws_url.clone(),
            self.chain_block_time,
            self.flashblock_block_time,
        ))
    }
}

impl<Node, Pool, Evm> PayloadServiceBuilder<Node, Pool, Evm> for CustomOpPayloadBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            Payload = OpEngineTypes,
            ChainSpec = OpChainSpec,
            Primitives = OpPrimitives,
        >,
    >,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
    <Pool as TransactionPool>::Transaction: OpPooledTx,
    Evm: ConfigureEvm<
            Primitives = PrimitivesTy<Node::Types>,
            NextBlockEnvCtx = OpNextBlockEnvAttributes,
        > + 'static,
{
    async fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: Evm,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypes>::Payload>> {
        tracing::info!("Spawning a custom payload builder");
        let extra_block_deadline = self.extra_block_deadline;
        let payload_builder = self.build_payload_builder(ctx, pool, evm_config).await?;
        let payload_job_config = BasicPayloadJobGeneratorConfig::default();

        let payload_generator = BlockPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            ctx.task_executor().clone(),
            payload_job_config,
            payload_builder,
            true,
            extra_block_deadline,
        );

        let (payload_service, payload_builder) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor()
            .spawn_critical("custom payload builder service", Box::pin(payload_service));

        tracing::info!("Custom payload service started");

        Ok(payload_builder)
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

/// Optimism's payload builder
#[derive(Debug, Clone)]
pub struct OpPayloadBuilder<Pool, Client> {
    /// The type responsible for creating the evm.
    pub evm_config: OpEvmConfig,
    /// The transaction pool
    pub pool: Pool,
    /// Node client
    pub client: Client,
    /// Channel sender for publishing messages
    pub tx: mpsc::UnboundedSender<String>,
    /// chain block time
    pub chain_block_time: u64,
    /// Flashblock block time
    pub flashblock_block_time: u64,
    /// Number of flashblocks per block
    pub flashblocks_per_block: u64,
    /// The metrics for the builder
    pub metrics: OpRBuilderMetrics,
}

impl<Pool, Client> OpPayloadBuilder<Pool, Client> {
    /// `OpPayloadBuilder` constructor.
    pub fn new(
        evm_config: OpEvmConfig,
        pool: Pool,
        client: Client,
        flashblocks_ws_url: String,
        chain_block_time: u64,
        flashblock_block_time: u64,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let subscribers = Arc::new(Mutex::new(Vec::new()));

        Self::publish_task(rx, subscribers.clone());

        tokio::spawn(async move {
            Self::start_ws(subscribers, &flashblocks_ws_url).await;
        });

        Self {
            evm_config,
            pool,
            client,
            tx,
            chain_block_time,
            flashblock_block_time,
            flashblocks_per_block: chain_block_time / flashblock_block_time,
            metrics: Default::default(),
        }
    }

    /// Start the WebSocket server
    pub async fn start_ws(subscribers: Arc<Mutex<Vec<WebSocketStream<TcpStream>>>>, addr: &str) {
        let listener = TcpListener::bind(addr).await.unwrap();
        let subscribers = subscribers.clone();

        tracing::info!("Starting WebSocket server on {}", addr);

        while let Ok((stream, _)) = listener.accept().await {
            tracing::info!("Accepted websocket connection");
            let subscribers = subscribers.clone();

            tokio::spawn(async move {
                match accept_async(stream).await {
                    Ok(ws_stream) => {
                        let mut subs = subscribers.lock().unwrap();
                        subs.push(ws_stream);
                    }
                    Err(e) => eprintln!("Error accepting websocket connection: {}", e),
                }
            });
        }
    }

    /// Background task that handles publishing messages to WebSocket subscribers
    fn publish_task(
        mut rx: mpsc::UnboundedReceiver<String>,
        subscribers: Arc<Mutex<Vec<WebSocketStream<TcpStream>>>>,
    ) {
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                let mut subscribers = subscribers.lock().unwrap();

                // Remove disconnected subscribers and send message to connected ones
                subscribers.retain_mut(|ws_stream| {
                    let message = message.clone();
                    async move {
                        ws_stream
                            .send(tokio_tungstenite::tungstenite::Message::Text(
                                message.into(),
                            ))
                            .await
                            .is_ok()
                    }
                    .now_or_never()
                    .unwrap_or(false)
                });
            }
        });
    }
}

impl<Pool, Client> OpPayloadBuilder<Pool, Client>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = OpTransactionSigned>>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthChainSpec + OpHardforks>,
{
    /// Send a message to be published
    pub fn send_message(&self, message: String) -> Result<(), Box<dyn std::error::Error>> {
        self.tx.send(message)?;
        self.metrics.messages_sent_count.increment(1);
        Ok(())
    }

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

        let mut info = execute_pre_steps(&mut db, &ctx)?;
        ctx.metrics
            .sequencer_tx_duration
            .record(sequencer_tx_start_time.elapsed());

        let (payload, fb_payload, mut bundle_state) = build_block(db, &ctx, &mut info)?;

        best_payload.set(payload.clone());
        let _ = self.send_message(serde_json::to_string(&fb_payload).unwrap_or_default());

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

        let gas_per_batch = ctx.block_gas_limit() / self.flashblocks_per_block;

        let mut total_gas_per_batch = gas_per_batch;

        let mut flashblock_count = 0;
        // Create a channel to coordinate flashblock building
        let (build_tx, mut build_rx) = mpsc::channel(1);

        // Spawn the timer task that signals when to build a new flashblock
        let cancel_clone = ctx.cancel.clone();
        let flashblock_block_time = self.flashblock_block_time;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(flashblock_block_time));
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
                    if flashblock_count >= self.flashblocks_per_block {
                        tracing::info!(
                            target: "payload_builder",
                            "Skipping flashblock reached target={} idx={}",
                            self.flashblocks_per_block,
                            flashblock_count
                        );
                        continue;
                    }

                    // Continue with flashblock building
                    tracing::info!(
                        target: "payload_builder",
                        "Building flashblock {} {}",
                        flashblock_count,
                        total_gas_per_batch,
                    );

                    let flashblock_build_start_time = Instant::now();
                    let state = StateProviderDatabase::new(&state_provider);

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

                            if let Err(err) = self.send_message(
                                serde_json::to_string(&fb_payload).unwrap_or_default(),
                            ) {
                                error!(target: "payload_builder", "Failed to send flashblock message: {}", err);
                            }

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

impl<Pool, Client> PayloadBuilder for OpPayloadBuilder<Pool, Client>
where
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthChainSpec + OpHardforks> + Clone,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = OpTransactionSigned>>,
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

pub fn build_block<ChainSpec, DB, P>(
    mut state: State<DB>,
    ctx: &OpPayloadBuilderCtx<ChainSpec>,
    info: &mut ExecutionInfo<OpPrimitives>,
) -> Result<(OpBuiltPayload, FlashblocksPayloadV1, BundleState), PayloadBuilderError>
where
    ChainSpec: EthChainSpec + OpHardforks,
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
    let new_transactions = info.executed_transactions[info.last_flashblock_index..].to_vec();

    let new_transactions_encoded = new_transactions
        .clone()
        .into_iter()
        .map(|tx| tx.encoded_2718().into())
        .collect::<Vec<_>>();

    let new_receipts = info.receipts[info.last_flashblock_index..].to_vec();
    info.last_flashblock_index = info.executed_transactions.len();
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

    let metadata: FlashblocksMetadata<OpPrimitives> = FlashblocksMetadata {
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
            withdrawals_root,
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

fn execute_pre_steps<ChainSpec, DB>(
    state: &mut State<DB>,
    ctx: &OpPayloadBuilderCtx<ChainSpec>,
) -> Result<ExecutionInfo<OpPrimitives>, PayloadBuilderError>
where
    ChainSpec: EthChainSpec + OpHardforks,
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

/// A type that returns a the [`PayloadTransactions`] that should be included in the pool.
pub trait OpPayloadTransactions<Transaction>: Clone + Send + Sync + Unpin + 'static {
    /// Returns an iterator that yields the transaction in the order they should get included in the
    /// new payload.
    fn best_transactions<Pool: TransactionPool<Transaction = Transaction>>(
        &self,
        pool: Pool,
        attr: BestTransactionsAttributes,
    ) -> impl PayloadTransactions<Transaction = Transaction>;
}

impl<T: PoolTransaction> OpPayloadTransactions<T> for () {
    fn best_transactions<Pool: TransactionPool<Transaction = T>>(
        &self,
        pool: Pool,
        attr: BestTransactionsAttributes,
    ) -> impl PayloadTransactions<Transaction = T> {
        BestPayloadTransactions::new(pool.best_transactions_with_attributes(attr))
    }
}

/// Container type that holds all necessities to build a new payload.
#[derive(Debug)]
pub struct OpPayloadBuilderCtx<ChainSpec> {
    /// The type that knows how to perform system calls and configure the evm.
    pub evm_config: OpEvmConfig,
    /// The chainspec
    pub chain_spec: Arc<ChainSpec>,
    /// How to build the payload.
    pub config: PayloadConfig<OpPayloadBuilderAttributes<OpTransactionSigned>>,
    /// Evm Settings
    pub evm_env: EvmEnv<OpSpecId>,
    /// Block env attributes for the current block.
    pub block_env_attributes: OpNextBlockEnvAttributes,
    /// Marker to check whether the job has been cancelled.
    pub cancel: CancellationToken,
    /// The metrics for the builder
    pub metrics: OpRBuilderMetrics,
}

impl<ChainSpec> OpPayloadBuilderCtx<ChainSpec>
where
    ChainSpec: EthChainSpec + OpHardforks,
{
    /// Returns the parent block the payload will be build on.
    pub fn parent(&self) -> &SealedHeader {
        &self.config.parent_header
    }

    /// Returns the builder attributes.
    pub const fn attributes(&self) -> &OpPayloadBuilderAttributes<OpTransactionSigned> {
        &self.config.attributes
    }

    /// Returns the withdrawals if shanghai is active.
    pub fn withdrawals(&self) -> Option<&Withdrawals> {
        self.chain_spec
            .is_shanghai_active_at_timestamp(self.attributes().timestamp())
            .then(|| &self.attributes().payload_attributes.withdrawals)
    }

    /// Returns the block gas limit to target.
    pub fn block_gas_limit(&self) -> u64 {
        self.attributes()
            .gas_limit
            .unwrap_or(self.evm_env.block_env.gas_limit)
    }

    /// Returns the block number for the block.
    pub fn block_number(&self) -> u64 {
        self.evm_env.block_env.number
    }

    /// Returns the current base fee
    pub fn base_fee(&self) -> u64 {
        self.evm_env.block_env.basefee
    }

    /// Returns the current blob gas price.
    pub fn get_blob_gasprice(&self) -> Option<u64> {
        self.evm_env
            .block_env
            .blob_gasprice()
            .map(|gasprice| gasprice as u64)
    }

    /// Returns the blob fields for the header.
    ///
    /// This will always return `Some(0)` after ecotone.
    pub fn blob_fields(&self) -> (Option<u64>, Option<u64>) {
        // OP doesn't support blobs/EIP-4844.
        // https://specs.optimism.io/protocol/exec-engine.html#ecotone-disable-blob-transactions
        // Need [Some] or [None] based on hardfork to match block hash.
        if self.is_ecotone_active() {
            (Some(0), Some(0))
        } else {
            (None, None)
        }
    }

    /// Returns the extra data for the block.
    ///
    /// After holocene this extracts the extradata from the paylpad
    pub fn extra_data(&self) -> Result<Bytes, PayloadBuilderError> {
        if self.is_holocene_active() {
            self.attributes()
                .get_holocene_extra_data(
                    self.chain_spec.base_fee_params_at_timestamp(
                        self.attributes().payload_attributes.timestamp,
                    ),
                )
                .map_err(PayloadBuilderError::other)
        } else {
            Ok(Default::default())
        }
    }

    /// Returns the current fee settings for transactions from the mempool
    pub fn best_transaction_attributes(&self) -> BestTransactionsAttributes {
        BestTransactionsAttributes::new(self.base_fee(), self.get_blob_gasprice())
    }

    /// Returns the unique id for this payload job.
    pub fn payload_id(&self) -> PayloadId {
        self.attributes().payload_id()
    }

    /// Returns true if regolith is active for the payload.
    pub fn is_regolith_active(&self) -> bool {
        self.chain_spec
            .is_regolith_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if ecotone is active for the payload.
    pub fn is_ecotone_active(&self) -> bool {
        self.chain_spec
            .is_ecotone_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if canyon is active for the payload.
    pub fn is_canyon_active(&self) -> bool {
        self.chain_spec
            .is_canyon_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if holocene is active for the payload.
    pub fn is_holocene_active(&self) -> bool {
        self.chain_spec
            .is_holocene_active_at_timestamp(self.attributes().timestamp())
    }
}

impl<ChainSpec> OpPayloadBuilderCtx<ChainSpec>
where
    ChainSpec: EthChainSpec + OpHardforks,
{
    /// Constructs a receipt for the given transaction.
    fn build_receipt<E: Evm>(
        &self,
        ctx: ReceiptBuilderCtx<'_, OpTransactionSigned, E>,
        deposit_nonce: Option<u64>,
    ) -> OpReceipt {
        let receipt_builder = self.evm_config.block_executor_factory().receipt_builder();
        match receipt_builder.build_receipt(ctx) {
            Ok(receipt) => receipt,
            Err(ctx) => {
                let receipt = alloy_consensus::Receipt {
                    // Success flag was added in `EIP-658: Embedding transaction status code
                    // in receipts`.
                    status: Eip658Value::Eip658(ctx.result.is_success()),
                    cumulative_gas_used: ctx.cumulative_gas_used,
                    logs: ctx.result.into_logs(),
                };

                receipt_builder.build_deposit_receipt(OpDepositReceipt {
                    inner: receipt,
                    deposit_nonce,
                    // The deposit receipt version was introduced in Canyon to indicate an
                    // update to how receipt hashes should be computed
                    // when set. The state transition process ensures
                    // this is only set for post-Canyon deposit
                    // transactions.
                    deposit_receipt_version: self.is_canyon_active().then_some(1),
                })
            }
        }
    }

    /// Executes all sequencer transactions that are included in the payload attributes.
    pub fn execute_sequencer_transactions<DB>(
        &self,
        db: &mut State<DB>,
    ) -> Result<ExecutionInfo<OpPrimitives>, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError>,
    {
        let mut info = ExecutionInfo::with_capacity(self.attributes().transactions.len());

        let mut evm = self.evm_config.evm_with_env(&mut *db, self.evm_env.clone());

        for sequencer_tx in &self.attributes().transactions {
            // A sequencer's block should never contain blob transactions.
            if sequencer_tx.value().is_eip4844() {
                return Err(PayloadBuilderError::other(
                    OpPayloadBuilderError::BlobTransactionRejected,
                ));
            }

            // Convert the transaction to a [Recovered<TransactionSigned>]. This is
            // purely for the purposes of utilizing the `evm_config.tx_env`` function.
            // Deposit transactions do not have signatures, so if the tx is a deposit, this
            // will just pull in its `from` address.
            let sequencer_tx = sequencer_tx
                .value()
                .clone()
                .try_clone_into_recovered()
                .map_err(|_| {
                    PayloadBuilderError::other(OpPayloadBuilderError::TransactionEcRecoverFailed)
                })?;

            // Cache the depositor account prior to the state transition for the deposit nonce.
            //
            // Note that this *only* needs to be done post-regolith hardfork, as deposit nonces
            // were not introduced in Bedrock. In addition, regular transactions don't have deposit
            // nonces, so we don't need to touch the DB for those.
            let depositor_nonce = (self.is_regolith_active() && sequencer_tx.is_deposit())
                .then(|| {
                    evm.db_mut()
                        .load_cache_account(sequencer_tx.signer())
                        .map(|acc| acc.account_info().unwrap_or_default().nonce)
                })
                .transpose()
                .map_err(|_| {
                    PayloadBuilderError::other(OpPayloadBuilderError::AccountLoadFailed(
                        sequencer_tx.signer(),
                    ))
                })?;

            let ResultAndState { result, state } = match evm.transact(&sequencer_tx) {
                Ok(res) => res,
                Err(err) => {
                    if err.is_invalid_tx_err() {
                        trace!(target: "payload_builder", %err, ?sequencer_tx, "Error in sequencer transaction, skipping.");
                        continue;
                    }
                    // this is an error that we should treat as fatal for this attempt
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            // add gas used by the transaction to cumulative gas used, before creating the receipt
            let gas_used = result.gas_used();
            info.cumulative_gas_used += gas_used;

            let ctx = ReceiptBuilderCtx {
                tx: sequencer_tx.inner(),
                evm: &evm,
                result,
                state: &state,
                cumulative_gas_used: info.cumulative_gas_used,
            };
            info.receipts.push(self.build_receipt(ctx, depositor_nonce));

            // commit changes
            evm.db_mut().commit(state);

            // append sender and transaction to the respective lists
            info.executed_senders.push(sequencer_tx.signer());
            info.executed_transactions.push(sequencer_tx.into_inner());
        }

        Ok(info)
    }

    /// Executes the given best transactions and updates the execution info.
    ///
    /// Returns `Ok(Some(())` if the job was cancelled.
    pub fn execute_best_transactions<DB>(
        &self,
        info: &mut ExecutionInfo<OpPrimitives>,
        db: &mut State<DB>,
        mut best_txs: impl PayloadTransactions<
            Transaction: PoolTransaction<Consensus = OpTransactionSigned>,
        >,
        batch_gas_limit: u64,
    ) -> Result<Option<()>, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError>,
    {
        let base_fee = self.base_fee();
        let mut num_txs_considered = 0;
        let mut num_txs_simulated = 0;
        let mut num_txs_simulated_success = 0;
        let mut num_txs_simulated_fail = 0;

        let mut evm = self.evm_config.evm_with_env(&mut *db, self.evm_env.clone());

        while let Some(tx) = best_txs.next(()) {
            let tx = tx.into_consensus();
            num_txs_considered += 1;

            // check in info if the txn has been executed already
            if info.executed_transactions.contains(&tx) {
                continue;
            }

            // ensure we still have capacity for this transaction
            if info.is_tx_over_limits(tx.inner(), batch_gas_limit, None, None) {
                // we can't fit this transaction into the block, so we need to mark it as
                // invalid which also removes all dependent transaction from
                // the iterator before we can continue
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            // A sequencer's block should never contain blob or deposit transactions from the pool.
            if tx.is_eip4844() || tx.is_deposit() {
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            // check if the job was cancelled, if so we can exit early
            if self.cancel.is_cancelled() {
                return Ok(Some(()));
            }

            let tx_simulation_start_time = Instant::now();
            let ResultAndState { result, state } = match evm.transact(&tx) {
                Ok(res) => res,
                Err(err) => {
                    if let Some(err) = err.as_invalid_tx_err() {
                        if err.is_nonce_too_low() {
                            // if the nonce is too low, we can skip this transaction
                            trace!(target: "payload_builder", %err, ?tx, "skipping nonce too low transaction");
                        } else {
                            // if the transaction is invalid, we can skip it and all of its
                            // descendants
                            trace!(target: "payload_builder", %err, ?tx, "skipping invalid transaction and its descendants");
                            best_txs.mark_invalid(tx.signer(), tx.nonce());
                        }

                        continue;
                    }
                    // this is an error that we should treat as fatal for this attempt
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            self.metrics
                .tx_simulation_duration
                .record(tx_simulation_start_time.elapsed());
            self.metrics.tx_byte_size.record(tx.inner().size() as f64);
            num_txs_simulated += 1;

            if result.is_success() {
                num_txs_simulated_success += 1;
            } else {
                num_txs_simulated_fail += 1;
                trace!(target: "payload_builder", ?tx, "reverted transaction");
            }

            // add gas used by the transaction to cumulative gas used, before creating the receipt
            let gas_used = result.gas_used();
            info.cumulative_gas_used += gas_used;

            let ctx = ReceiptBuilderCtx {
                tx: tx.inner(),
                evm: &evm,
                result,
                state: &state,
                cumulative_gas_used: info.cumulative_gas_used,
            };
            info.receipts.push(self.build_receipt(ctx, None));

            // commit changes
            evm.db_mut().commit(state);

            // update add to total fees
            let miner_fee = tx
                .effective_tip_per_gas(base_fee)
                .expect("fee is always valid; execution succeeded");
            info.total_fees += U256::from(miner_fee) * U256::from(gas_used);

            // append sender and transaction to the respective lists
            info.executed_senders.push(tx.signer());
            info.executed_transactions.push(tx.into_inner());
        }

        self.metrics
            .payload_num_tx_considered
            .record(num_txs_considered as f64);
        self.metrics
            .payload_num_tx_simulated
            .record(num_txs_simulated as f64);
        self.metrics
            .payload_num_tx_simulated_success
            .record(num_txs_simulated_success as f64);
        self.metrics
            .payload_num_tx_simulated_fail
            .record(num_txs_simulated_fail as f64);

        Ok(None)
    }
}
