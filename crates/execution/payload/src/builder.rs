//! Base payload builder implementation.
use std::{marker::PhantomData, sync::Arc};

use alloy_consensus::{BlockHeader, Transaction, Typed2718};
use alloy_evm::Evm as AlloyEvm;
use alloy_primitives::{B256, U256};
use alloy_rpc_types_debug::ExecutionWitness;
use alloy_rpc_types_engine::PayloadId;
use base_common_chains::Upgrades;
use base_common_consensus::{BaseTransaction, Predeploys};
use base_common_evm::L1BlockInfo;
use base_execution_eip8130::IntrinsicGas;
use base_execution_txpool::{BasePooledTx, GuardMetrics, estimated_da_size::DataAvailabilitySized};
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, BuildOutcomeKind, MissingPayloadBehaviour, PayloadBuilder,
    PayloadConfig, is_better_payload,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_evm::{
    BlockExecutorForEvm, ConfigureEvm, Database,
    execute::{
        BlockBuilder, BlockBuilderOutcome, BlockExecutionError, BlockExecutor, BlockValidationError,
    },
};
use reth_execution_cache::{CachedStateMetrics, CachedStateMetricsSource, CachedStateProvider};
use reth_execution_types::BlockExecutionOutput;
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::{BuildNextEnv, BuiltPayloadExecutedBlock};
use reth_payload_util::{BestPayloadTransactions, NoopPayloadTransactions, PayloadTransactions};
use reth_primitives_traits::{
    HeaderTy, NodePrimitives, SealedHeader, SealedHeaderFor, SignedTransaction, TxTy,
};
use reth_revm::{
    cancelled::CancelOnDrop, database::StateProviderDatabase, db::State,
    witness::ExecutionWitnessRecord,
};
use reth_storage_api::{BlockReader, StateProvider, StateProviderFactory, errors::ProviderError};
use reth_transaction_pool::{BestTransactionsAttributes, PoolTransaction, TransactionPool};
use reth_trie_common::ExecutionWitnessMode;
use reth_trie_parallel::state_root_task::PayloadStateRootHandle;
use revm::context::{Block, BlockEnv};
use tracing::{debug, debug_span, instrument, trace, warn};

use crate::{
    Attributes, BasePayloadBuilderAttributes, PayloadPrimitives, ResourceMeteringDecision,
    ResourceMeteringMetrics, config::BaseBuilderConfig, error::BasePayloadBuilderError,
    evaluate_transaction, payload::BaseBuiltPayload,
};

/// Base payload builder
#[derive(Debug)]
pub struct BasePayloadBuilder<
    Pool,
    Client,
    Evm,
    Txs = (),
    Attrs = BasePayloadBuilderAttributes<TxTy<<Evm as ConfigureEvm>::Primitives>>,
> {
    /// The type responsible for creating the evm.
    pub evm_config: Evm,
    /// Transaction pool.
    pub pool: Pool,
    /// Node client.
    pub client: Client,
    /// Settings for the builder, e.g. DA settings.
    pub config: BaseBuilderConfig,
    /// The type responsible for yielding the best transactions for the payload if mempool
    /// transactions are allowed.
    pub best_transactions: Txs,
    /// Marker for the payload attributes type.
    _pd: PhantomData<Attrs>,
}

impl<Pool, Client, Evm, Txs, Attrs> Clone for BasePayloadBuilder<Pool, Client, Evm, Txs, Attrs>
where
    Pool: Clone,
    Client: Clone,
    Evm: ConfigureEvm,
    Txs: Clone,
{
    fn clone(&self) -> Self {
        Self {
            evm_config: self.evm_config.clone(),
            pool: self.pool.clone(),
            client: self.client.clone(),
            config: self.config.clone(),
            best_transactions: self.best_transactions.clone(),
            _pd: PhantomData,
        }
    }
}

impl<Pool, Client, Evm, Attrs> BasePayloadBuilder<Pool, Client, Evm, (), Attrs> {
    /// `BasePayloadBuilder` constructor.
    ///
    /// Configures the builder with the default settings.
    pub fn new(pool: Pool, client: Client, evm_config: Evm) -> Self {
        Self::with_builder_config(pool, client, evm_config, Default::default())
    }

    /// Configures the builder with the given [`BaseBuilderConfig`].
    pub const fn with_builder_config(
        pool: Pool,
        client: Client,
        evm_config: Evm,
        config: BaseBuilderConfig,
    ) -> Self {
        Self { pool, client, evm_config, config, best_transactions: (), _pd: PhantomData }
    }
}

impl<Pool, Client, Evm, Txs, Attrs> BasePayloadBuilder<Pool, Client, Evm, Txs, Attrs> {
    /// Configures the type responsible for yielding the transactions that should be included in the
    /// payload.
    pub fn with_transactions<T>(
        self,
        best_transactions: T,
    ) -> BasePayloadBuilder<Pool, Client, Evm, T, Attrs> {
        BasePayloadBuilder {
            pool: self.pool,
            client: self.client,
            evm_config: self.evm_config,
            best_transactions,
            config: self.config,
            _pd: PhantomData,
        }
    }
}

impl<Pool, Client, Evm, N, T, Attrs> BasePayloadBuilder<Pool, Client, Evm, T, Attrs>
where
    Pool: TransactionPool<Transaction: BasePooledTx<Consensus = N::SignedTx>>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: Upgrades> + BlockReader,
    N: PayloadPrimitives,
    Evm: ConfigureEvm<
            Primitives = N,
            NextBlockEnvCtx: BuildNextEnv<Attrs, N::BlockHeader, Client::ChainSpec>,
        >,
    Attrs: Attributes<Transaction = TxTy<Evm::Primitives>>,
{
    /// Constructs a Base payload from the transactions sent via the
    /// Payload attributes by the sequencer. If the `no_tx_pool` argument is passed in
    /// the payload attributes, the transaction pool will be ignored and the only transactions
    /// included in the payload will be those sent through the attributes.
    ///
    /// Given build arguments including a Base client, transaction pool,
    /// and configuration, this function creates a transaction payload. Returns
    /// a result indicating success with the payload or an error in case of failure.
    #[instrument(
        skip_all,
        fields(payload_id = tracing::field::Empty, parent_num = tracing::field::Empty)
    )]
    fn build_payload<'a, Txs>(
        &self,
        args: BuildArguments<Attrs, BaseBuiltPayload<N>>,
        best: impl FnOnce(BestTransactionsAttributes) -> Txs + Send + Sync + 'a,
    ) -> Result<BuildOutcome<BaseBuiltPayload<N>>, PayloadBuilderError>
    where
        Txs: PayloadTransactions<
            Transaction: PoolTransaction<Consensus = N::SignedTx> + BasePooledTx,
        >,
    {
        let BuildArguments {
            mut cached_reads,
            execution_cache,
            state_root_handle,
            config,
            cancel,
            best_payload,
        } = args;

        let ctx = BasePayloadBuilderCtx {
            evm_config: self.evm_config.clone(),
            builder_config: self.config.clone(),
            chain_spec: self.client.chain_spec(),
            config,
            cancel,
            best_payload,
        };
        tracing::Span::current().record("payload_id", tracing::field::display(ctx.payload_id()));
        tracing::Span::current().record("parent_num", ctx.parent().number());

        let builder = Builder::new(best);

        let mut state_provider = self.client.state_by_block_hash(ctx.parent().hash())?;
        if let Some(execution_cache) = execution_cache {
            state_provider = Box::new(CachedStateProvider::new(
                state_provider,
                execution_cache.cache().clone(),
                Some(CachedStateMetrics::zeroed(CachedStateMetricsSource::Builder)),
            ));
        }
        let state = StateProviderDatabase::new(state_provider.as_ref());

        if ctx.attributes().no_tx_pool() {
            builder.build(state, state_provider.as_ref(), state_root_handle, ctx)
        } else {
            // sequencer mode we can reuse cachedreads from previous runs
            builder.build(
                cached_reads.as_db_mut(state),
                state_provider.as_ref(),
                state_root_handle,
                ctx,
            )
        }
        .map(|out| out.with_cached_reads(cached_reads))
    }

    /// Computes the witness for the payload.
    pub fn payload_witness(
        &self,
        parent: SealedHeader<N::BlockHeader>,
        attributes: Attrs::RpcPayloadAttributes,
    ) -> Result<ExecutionWitness, PayloadBuilderError>
    where
        Attrs: Attributes,
    {
        let attributes =
            Attrs::try_new(parent.hash(), attributes, 3).map_err(PayloadBuilderError::other)?;

        let payload_id = attributes.payload_id(&parent.hash());
        let config = PayloadConfig::new(Arc::new(parent), attributes, payload_id);
        let ctx = BasePayloadBuilderCtx {
            evm_config: self.evm_config.clone(),
            builder_config: self.config.clone(),
            chain_spec: self.client.chain_spec(),
            config,
            cancel: Default::default(),
            best_payload: Default::default(),
        };

        let state_provider = self.client.state_by_block_hash(ctx.parent().hash())?;

        let builder = Builder::new(|_| NoopPayloadTransactions::<Pool::Transaction>::default());
        builder.witness(state_provider, &self.client, &ctx)
    }
}

/// Implementation of the [`PayloadBuilder`] trait for [`BasePayloadBuilder`].
impl<Pool, Client, Evm, N, Txs, Attrs> PayloadBuilder
    for BasePayloadBuilder<Pool, Client, Evm, Txs, Attrs>
where
    N: PayloadPrimitives,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: Upgrades> + BlockReader + Clone,
    Pool: TransactionPool<Transaction: BasePooledTx<Consensus = N::SignedTx>>,
    Evm: ConfigureEvm<
            Primitives = N,
            NextBlockEnvCtx: BuildNextEnv<Attrs, N::BlockHeader, Client::ChainSpec>,
        >,
    Txs: BasePayloadTransactions<Pool::Transaction>,
    Attrs: Attributes<Transaction = N::SignedTx>,
{
    type Attributes = Attrs;
    type BuiltPayload = BaseBuiltPayload<N>;

    fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        let pool = self.pool.clone();
        self.build_payload(args, |attrs| self.best_transactions.best_transactions(pool, attrs))
    }

    fn on_missing_payload(
        &self,
        _args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        // we want to await the job that's already in progress because that should be returned as
        // is, there's no benefit in racing another job
        MissingPayloadBehaviour::AwaitInProgress
    }

    // NOTE: this should only be used for testing purposes because this doesn't have access to L1
    // system txs, hence on_missing_payload we return [MissingPayloadBehaviour::AwaitInProgress].
    fn build_empty_payload(
        &self,
        config: PayloadConfig<Self::Attributes, N::BlockHeader>,
    ) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        let args = BuildArguments {
            config,
            cached_reads: Default::default(),
            execution_cache: None,
            state_root_handle: None,
            cancel: Default::default(),
            best_payload: None,
        };
        self.build_payload(args, |_| NoopPayloadTransactions::<Pool::Transaction>::default())?
            .into_payload()
            .ok_or_else(|| PayloadBuilderError::MissingPayload)
    }
}

/// The type that builds the payload.
///
/// Payload building for Base is composed of several steps.
/// The first steps are mandatory and defined by the protocol.
///
/// 1. first all System calls are applied.
/// 2. After canyon the forced deployed `create2deployer` must be loaded
/// 3. all sequencer transactions are executed (part of the payload attributes)
///
/// Depending on whether the node acts as a sequencer and is allowed to include additional
/// transactions (`no_tx_pool == false`):
/// 4. include additional transactions
///
/// And finally
/// 5. build the block: compute all roots (txs, state)
#[derive(derive_more::Debug)]
pub struct Builder<'a, Txs> {
    /// Yields the best transaction to include if transactions from the mempool are allowed.
    #[debug(skip)]
    best: Box<dyn FnOnce(BestTransactionsAttributes) -> Txs + 'a>,
}

impl<'a, Txs> Builder<'a, Txs> {
    /// Creates a new [`Builder`].
    pub fn new(best: impl FnOnce(BestTransactionsAttributes) -> Txs + Send + Sync + 'a) -> Self {
        Self { best: Box::new(best) }
    }
}

impl<Txs> Builder<'_, Txs> {
    /// Builds the payload on top of the state.
    pub fn build<Evm, ChainSpec, N, Attrs>(
        self,
        db: impl Database<Error = ProviderError>,
        state_provider: &dyn StateProvider,
        mut state_root_handle: Option<PayloadStateRootHandle>,
        ctx: BasePayloadBuilderCtx<Evm, ChainSpec, Attrs>,
    ) -> Result<BuildOutcomeKind<BaseBuiltPayload<N>>, PayloadBuilderError>
    where
        Evm: ConfigureEvm<
                Primitives = N,
                NextBlockEnvCtx: BuildNextEnv<Attrs, N::BlockHeader, ChainSpec>,
            >,
        ChainSpec: EthChainSpec + Upgrades,
        N: PayloadPrimitives,
        Txs: PayloadTransactions<
            Transaction: PoolTransaction<Consensus = N::SignedTx> + BasePooledTx,
        >,
        Attrs: Attributes<Transaction = N::SignedTx>,
    {
        let Self { best } = self;
        debug!(target: "payload_builder", id=%ctx.payload_id(), parent_header = ?ctx.parent().hash(), parent_number = ctx.parent().number(), "building new payload");

        let mut db = State::builder().with_database(db).with_bundle_update().build();

        // Load the L1 block contract into the database cache. If the L1 block contract is not
        // pre-loaded the database will panic when trying to fetch the DA footprint gas
        // scalar.
        db.load_cache_account(Predeploys::L1_BLOCK_INFO).map_err(BlockExecutionError::other)?;

        let mut builder = ctx.block_builder(&mut db)?;

        if let Some(task) = state_root_handle.as_mut() {
            builder.evm_mut().db_mut().set_state_hook(Some(Box::new(task.take_state_hook())));
        }

        // 1. apply pre-execution changes
        builder.apply_pre_execution_changes().map_err(|err| {
            warn!(target: "payload_builder", %err, "failed to apply pre-execution changes");
            PayloadBuilderError::Internal(err.into())
        })?;

        // 2. execute sequencer transactions
        let mut info = ctx.execute_sequencer_transactions(&mut builder)?;

        // 3. if mem pool transactions are requested we execute them
        if !ctx.attributes().no_tx_pool() {
            let best_txs = best(ctx.best_transaction_attributes(builder.evm_mut().block()));
            if ctx.execute_best_transactions(&mut info, &mut builder, best_txs)?.is_some() {
                return Ok(BuildOutcomeKind::Cancelled);
            }

            // check if the new payload is even more valuable
            if !ctx.is_denim_active() && !ctx.is_better_payload(info.total_fees) {
                // can skip building the block
                return Ok(BuildOutcomeKind::Aborted { fees: info.total_fees });
            }
        }

        let block_num = ctx.parent().number().saturating_add(1);
        let state_root = state_root_handle.and_then(|mut task| {
            // Dropping the hook closes the update stream so the parallel task can finish.
            builder.evm_mut().db_mut().set_state_hook(None);
            match task.state_root() {
                Ok(outcome) => {
                    debug!(
                        target: "payload_builder",
                        id = %ctx.payload_id(),
                        state_root = ?outcome.state_root,
                        job = task.name(),
                        "received state root from state-root job"
                    );
                    Some((outcome.state_root, Arc::unwrap_or_clone(outcome.trie_updates)))
                }
                Err(error) => {
                    warn!(
                        target: "payload_builder",
                        id = %ctx.payload_id(),
                        error = %error,
                        "state-root job failed, falling back to synchronous state root"
                    );
                    None
                }
            }
        });
        let BlockBuilderOutcome {
            execution_result,
            hashed_state,
            trie_updates,
            block,
            block_access_list,
        } = debug_span!("finish_payload", block_num)
            .in_scope(|| builder.finish(state_provider, state_root))?;

        let sealed_block = Arc::new(block.sealed_block().clone());
        debug!(target: "payload_builder", id=%ctx.payload_id(), sealed_block_header = ?sealed_block.header(), "sealed built block");

        let execution_outcome =
            BlockExecutionOutput { state: db.take_bundle(), result: execution_result };

        // create the executed block data
        let executed: BuiltPayloadExecutedBlock<N> = BuiltPayloadExecutedBlock {
            recovered_block: Arc::new(block),
            execution_output: Arc::new(execution_outcome),
            hashed_state: Arc::new(hashed_state),
            trie_updates: Arc::new(trie_updates),
        };

        let no_tx_pool = ctx.attributes().no_tx_pool();

        let payload = BaseBuiltPayload::new(
            ctx.payload_id(),
            sealed_block,
            info.total_fees,
            Some(executed),
            block_access_list.map(|bal| alloy_rlp::encode(bal).into()),
        );

        if no_tx_pool || ctx.is_denim_active() {
            // if `no_tx_pool` is set only transactions from the payload attributes will be included
            // in the payload. In other words, the payload is deterministic and we can
            // freeze it once we've successfully built it.
            // Denim-active sequencer builds are one-shot, so this payload is also final.
            Ok(BuildOutcomeKind::Freeze(payload))
        } else {
            Ok(BuildOutcomeKind::Better { payload })
        }
    }

    /// Builds the payload and returns its [`ExecutionWitness`] based on the state after execution.
    pub fn witness<Evm, ChainSpec, N, Attrs>(
        self,
        state_provider: impl StateProvider,
        header_provider: impl reth_storage_api::HeaderProvider,
        ctx: &BasePayloadBuilderCtx<Evm, ChainSpec, Attrs>,
    ) -> Result<ExecutionWitness, PayloadBuilderError>
    where
        Evm: ConfigureEvm<
                Primitives = N,
                NextBlockEnvCtx: BuildNextEnv<Attrs, N::BlockHeader, ChainSpec>,
            >,
        ChainSpec: EthChainSpec + Upgrades,
        N: PayloadPrimitives,
        Txs: PayloadTransactions<Transaction: PoolTransaction<Consensus = N::SignedTx>>,
        Attrs: Attributes<Transaction = N::SignedTx>,
    {
        let mut db = State::builder()
            .with_database(StateProviderDatabase::new(&state_provider))
            .with_bundle_update()
            .build();
        let mut builder = ctx.block_builder(&mut db)?;
        let block_number =
            builder.evm().block().number().try_into().expect("block_number must be < u64::MAX");

        builder.apply_pre_execution_changes()?;
        ctx.execute_sequencer_transactions(&mut builder)?;
        builder.into_executor().apply_post_execution_changes()?;

        if ctx.chain_spec.is_isthmus_active_at_timestamp(ctx.attributes().timestamp()) {
            // force load `L2ToL1MessagePasser.sol` so l2 withdrawals root can be computed even if
            // no l2 withdrawals in block
            _ = db.load_cache_account(Predeploys::L2_TO_L1_MESSAGE_PASSER)?;
        }

        let mode = ExecutionWitnessMode::default();
        let witness = ExecutionWitnessRecord::new(&db).into_execution_witness(
            &db.database.0,
            &header_provider,
            block_number,
            mode,
        )?;
        Ok(witness)
    }
}

/// A type that returns the [`PayloadTransactions`] that should be included in the pool.
pub trait BasePayloadTransactions<Transaction>: Clone + Send + Sync + Unpin + 'static {
    /// Returns an iterator that yields the transaction in the order they should get included in the
    /// new payload.
    fn best_transactions<Pool: TransactionPool<Transaction = Transaction>>(
        &self,
        pool: Pool,
        attr: BestTransactionsAttributes,
    ) -> impl PayloadTransactions<Transaction = Transaction>;
}

impl<T: PoolTransaction> BasePayloadTransactions<T> for () {
    fn best_transactions<Pool: TransactionPool<Transaction = T>>(
        &self,
        pool: Pool,
        attr: BestTransactionsAttributes,
    ) -> impl PayloadTransactions<Transaction = T> {
        BestPayloadTransactions::new(pool.best_transactions_with_attributes(attr))
    }
}

/// Holds the state after execution
#[derive(Debug)]
pub struct ExecutedPayload<N: NodePrimitives> {
    /// Tracked execution info
    pub info: ExecutionInfo,
    /// Withdrawal hash.
    pub withdrawals_root: Option<B256>,
    /// The transaction receipts.
    pub receipts: Vec<N::Receipt>,
    /// The block env used during execution.
    pub block_env: BlockEnv,
}

/// This acts as the container for executed transactions and its byproducts (receipts, gas used)
#[derive(Default, Debug)]
pub struct ExecutionInfo {
    /// All gas used so far
    pub cumulative_gas_used: u64,
    /// Estimated DA size
    pub cumulative_da_bytes_used: u64,
    /// Tracks fees from executed mempool transactions
    pub total_fees: U256,
    /// Cumulative resource-metering units for the current payload, aligned with the snapped schedule.
    pub resource_metering_usage: Vec<u128>,
}

impl ExecutionInfo {
    /// Create a new instance with allocated slots.
    pub const fn new() -> Self {
        Self {
            cumulative_gas_used: 0,
            cumulative_da_bytes_used: 0,
            total_fees: U256::ZERO,
            resource_metering_usage: Vec::new(),
        }
    }

    /// Returns true if the transaction would exceed the block limits:
    /// - block gas limit: ensures the transaction still fits into the block. `tx_reserved_gas` is
    ///   the gas reserved against the block budget: `gas_limit` for ordinary transactions, and
    ///   `gas_limit + payer_auth` for EIP-8130, since payer authentication is metered on top of the
    ///   declared gas limit (see `IntrinsicGas::max_payer_auth_cost`).
    /// - tx DA limit: if configured, ensures the tx does not exceed the maximum allowed DA limit
    ///   per tx.
    /// - block DA limit: if configured, ensures the transaction's DA size does not exceed the
    ///   maximum allowed DA limit per block.
    pub fn is_tx_over_limits(
        &self,
        tx_da_size: u64,
        block_gas_limit: u64,
        tx_data_limit: Option<u64>,
        block_data_limit: Option<u64>,
        tx_reserved_gas: u64,
        da_footprint_gas_scalar: Option<u16>,
    ) -> bool {
        if tx_data_limit.is_some_and(|da_limit| tx_da_size > da_limit) {
            return true;
        }

        let total_da_bytes_used = self.cumulative_da_bytes_used.saturating_add(tx_da_size);

        if block_data_limit.is_some_and(|da_limit| total_da_bytes_used > da_limit) {
            return true;
        }

        // Post Jovian: the tx DA footprint must be less than the block gas limit
        if let Some(da_footprint_gas_scalar) = da_footprint_gas_scalar {
            let tx_da_footprint =
                total_da_bytes_used.saturating_mul(da_footprint_gas_scalar as u64);
            if tx_da_footprint > block_gas_limit {
                return true;
            }
        }

        self.cumulative_gas_used.saturating_add(tx_reserved_gas) > block_gas_limit
    }
}

/// Container type that holds all necessities to build a new payload.
#[derive(derive_more::Debug)]
pub struct BasePayloadBuilderCtx<
    Evm: ConfigureEvm,
    ChainSpec,
    Attrs = BasePayloadBuilderAttributes<TxTy<<Evm as ConfigureEvm>::Primitives>>,
> {
    /// The type that knows how to perform system calls and configure the evm.
    pub evm_config: Evm,
    /// Additional config for the builder/sequencer, e.g. DA and gas limit
    pub builder_config: BaseBuilderConfig,
    /// The chainspec
    pub chain_spec: Arc<ChainSpec>,
    /// How to build the payload.
    pub config: PayloadConfig<Attrs, HeaderTy<Evm::Primitives>>,
    /// Marker to check whether the job has been cancelled.
    pub cancel: CancelOnDrop,
    /// The currently best payload.
    pub best_payload: Option<BaseBuiltPayload<Evm::Primitives>>,
}

impl<Evm, ChainSpec, Attrs> BasePayloadBuilderCtx<Evm, ChainSpec, Attrs>
where
    Evm: ConfigureEvm<
            Primitives: PayloadPrimitives,
            NextBlockEnvCtx: BuildNextEnv<Attrs, HeaderTy<Evm::Primitives>, ChainSpec>,
        >,
    ChainSpec: EthChainSpec + Upgrades,
    Attrs: Attributes<Transaction = TxTy<Evm::Primitives>>,
{
    /// Returns the parent block the payload will be build on.
    pub fn parent(&self) -> &SealedHeaderFor<Evm::Primitives> {
        self.config.parent_header.as_ref()
    }

    /// Returns the builder attributes.
    pub const fn attributes(&self) -> &Attrs {
        &self.config.attributes
    }

    /// Returns `true` if Denim is active at this payload's timestamp.
    pub fn is_denim_active(&self) -> bool {
        self.chain_spec.is_denim_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns the current fee settings for transactions from the mempool
    pub fn best_transaction_attributes(&self, block_env: impl Block) -> BestTransactionsAttributes {
        BestTransactionsAttributes::new(
            block_env.basefee(),
            block_env.blob_gasprice().map(|p| p as u64),
        )
    }

    /// Returns the unique id for this payload job.
    pub const fn payload_id(&self) -> PayloadId {
        self.config.payload_id()
    }

    /// Returns true if the fees are higher than the previous payload.
    pub fn is_better_payload(&self, total_fees: U256) -> bool {
        is_better_payload(self.best_payload.as_ref(), total_fees)
    }

    /// Prepares a [`BlockBuilder`] for the next block.
    pub fn block_builder<'a, DB: Database>(
        &'a self,
        db: &'a mut State<DB>,
    ) -> Result<
        impl BlockBuilder<Primitives = Evm::Primitives, Executor = BlockExecutorForEvm<'a, Evm, DB>>
        + 'a,
        PayloadBuilderError,
    > {
        self.evm_config
            .builder_for_next_block(
                db,
                self.parent(),
                Evm::NextBlockEnvCtx::build_next_env(
                    self.attributes(),
                    self.parent(),
                    self.chain_spec.as_ref(),
                )
                .map_err(PayloadBuilderError::other)?,
            )
            .map_err(PayloadBuilderError::other)
    }

    /// Executes all sequencer transactions that are included in the payload attributes.
    ///
    /// When `no_tx_pool` is set the attribute-supplied transaction list is the consensus input
    /// for the payload (derived from L1 batches by `base-consensus`), not a list of optional
    /// pre-include candidates. In that mode an `InvalidTx` from any sequencer transaction must
    /// be propagated as a fatal error so the EL rejects the payload, matching the strictness of
    /// the proof executor. Silently skipping the offending transaction would diverge the EL
    /// safe-head from the proof-derived state and break Holocene's deposit-only fallback (the
    /// EL would freeze a skip-and-continue block while the proof path produces a deposit-only
    /// replacement root).
    ///
    /// When `no_tx_pool` is `false` the builder is composing a new block from mempool plus
    /// attribute pre-includes; pre-includes there may legitimately be skipped on `InvalidTx`,
    /// so the historical skip-and-continue behavior is preserved.
    #[instrument(skip_all, fields(phase = "sequencer_txs"))]
    pub fn execute_sequencer_transactions(
        &self,
        builder: &mut impl BlockBuilder<Primitives = Evm::Primitives>,
    ) -> Result<ExecutionInfo, PayloadBuilderError> {
        let mut info = ExecutionInfo::new();
        let no_tx_pool = self.attributes().no_tx_pool();

        for sequencer_tx in self.attributes().sequencer_transactions() {
            // A sequencer's block should never contain blob transactions.
            if sequencer_tx.value().is_eip4844() {
                return Err(PayloadBuilderError::other(
                    BasePayloadBuilderError::BlobTransactionRejected,
                ));
            }

            // Convert the transaction to a [RecoveredTx]. This is
            // purely for the purposes of utilizing the `evm_config.tx_env`` function.
            // Deposit transactions do not have signatures, so if the tx is a deposit, this
            // will just pull in its `from` address.
            let sequencer_tx = sequencer_tx.value().try_clone_into_recovered().map_err(|_| {
                PayloadBuilderError::other(BasePayloadBuilderError::TransactionEcRecoverFailed)
            })?;

            let gas_output = match builder.execute_transaction(sequencer_tx.clone()) {
                Ok(gas_output) => gas_output,
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
                })) if !no_tx_pool => {
                    trace!(target: "payload_builder", %error, ?sequencer_tx, "Error in sequencer transaction, skipping.");
                    continue;
                }
                Err(err) => {
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            info.cumulative_gas_used += gas_output.tx_gas_used();
        }

        Ok(info)
    }

    /// Executes the given best transactions and updates the execution info.
    ///
    /// Returns `Ok(Some(()))` if the job was cancelled.
    #[instrument(skip_all, fields(phase = "mempool_txs"))]
    pub fn execute_best_transactions<Builder>(
        &self,
        info: &mut ExecutionInfo,
        builder: &mut Builder,
        mut best_txs: impl PayloadTransactions<
            Transaction: PoolTransaction<Consensus = TxTy<Evm::Primitives>> + BasePooledTx,
        >,
    ) -> Result<Option<()>, PayloadBuilderError>
    where
        Builder: BlockBuilder<Primitives = Evm::Primitives>,
        <<Builder::Executor as BlockExecutor>::Evm as AlloyEvm>::DB: Database,
    {
        let gas_limit = builder.evm_mut().block().gas_limit();
        // If a gas limit is configured, use that limit as target if it's smaller, otherwise use
        // the block's actual gas limit.
        let block_gas_limit = self
            .builder_config
            .gas_limit_config
            .gas_limit()
            .map_or(gas_limit, |cfg| cfg.min(gas_limit));
        let block_da_limit = self.builder_config.da_config.max_da_block_size();
        let tx_da_limit = self.builder_config.da_config.max_da_tx_size();
        let base_fee = builder.evm_mut().block().basefee();

        let block_timestamp = self.attributes().timestamp();
        let can_finalize_early = self.is_denim_active();
        let resource_metering = &self.builder_config.resource_metering;
        let resource_schedule = resource_metering.store.snapshot();
        let resource_metering_active =
            resource_metering.mode.is_enabled() && !resource_schedule.is_empty();
        while let Some(tx) = best_txs.next(()) {
            if self.cancel.is_cancelled() {
                return Ok(Some(()));
            }
            if can_finalize_early && self.cancel.is_finalization_requested() {
                break;
            }

            if self.builder_config.manifest_precheck_enabled
                && let Some(manifest) = tx.watch_manifest()
                && let Err(stale) = manifest.revalidate(builder.evm_mut().db_mut(), block_timestamp)
            {
                trace!(
                    target: "payload_builder",
                    tx_hash = ?tx.hash(),
                    cause = stale.cause(),
                    "skipping EIP-8130 transaction with stale authorization manifest"
                );
                GuardMetrics::record_builder_precheck_drop(&stale);
                // Nonce-free replay-ID entries are independent. The upstream
                // payload adapter invalidates by sender (not by replay ID), so
                // marking one would suppress unrelated entries from this sender.
                // This transaction has already been consumed from the iterator.
                if tx.eip8130_replay_id().is_none() {
                    best_txs.mark_invalid(tx.sender(), tx.nonce());
                }
                continue;
            }

            let tx_da_size = tx.estimated_da_size();

            // EIP-8130 meters payer authentication gas on top of the declared gas limit, so it must
            // be reserved against the block gas budget in addition to `gas_limit`. Reserve a
            // conservative upper bound (worst-case payer policy gate) derived from the payer auth
            // blob (`0` for non-8130 / self-pay); see `IntrinsicGas::max_payer_auth_cost`.
            let tx_payer_auth = match tx.as_eip8130() {
                Some(signed) => match IntrinsicGas::max_payer_auth_cost(signed) {
                    Ok(payer_auth) => payer_auth,
                    Err(err) => {
                        trace!(
                            target: "payload_builder",
                            %err,
                            tx_hash = ?tx.hash(),
                            "skipping EIP-8130 transaction with unschedulable payer authenticator"
                        );
                        // Mirror the manifest pre-check above: a nonce-free replay-ID entry is
                        // independent, so invalidating by sender would suppress unrelated entries.
                        if tx.eip8130_replay_id().is_none() {
                            best_txs.mark_invalid(tx.sender(), tx.nonce());
                        }
                        continue;
                    }
                },
                None => 0,
            };

            let tx_hash = *tx.hash();
            let mut pending_resource_usage = None;
            if resource_metering_active {
                let meter =
                    crate::MeteringProvider::get(resource_metering.provider.as_ref(), &tx_hash);

                match evaluate_transaction(
                    resource_schedule.as_ref(),
                    meter.as_ref(),
                    &tx_hash,
                    &info.resource_metering_usage,
                ) {
                    ResourceMeteringDecision::Inactive => {}
                    ResourceMeteringDecision::Allow(usage) => {
                        pending_resource_usage = Some(usage);
                    }
                    ResourceMeteringDecision::Reject { error, usage } => {
                        let enforced = !resource_metering.mode.is_dry_run();
                        ResourceMeteringMetrics::record_limit(&error, enforced);
                        warn!(
                            target: "payload_builder",
                            tx_hash = %tx_hash,
                            dimension = %error.dimension,
                            scope = %error.scope,
                            used = error.used,
                            limit = error.limit,
                            dry_run = !enforced,
                            "resource metering budget exceeded"
                        );
                        if enforced {
                            best_txs.mark_invalid(tx.sender(), tx.nonce());
                            continue;
                        }
                        pending_resource_usage = Some(usage);
                    }
                    ResourceMeteringDecision::CalculationFailed => {
                        ResourceMeteringMetrics::calculation_failed().increment(1);
                        warn!(
                            target: "payload_builder",
                            tx_hash = %tx_hash,
                            "resource metering usage calculation failed"
                        );
                        best_txs.mark_invalid(tx.sender(), tx.nonce());
                        continue;
                    }
                }
            }

            let tx = tx.into_consensus();

            let da_footprint_gas_scalar = self
                .chain_spec
                .is_jovian_active_at_timestamp(self.attributes().timestamp())
                .then_some(
                    L1BlockInfo::fetch_da_footprint_gas_scalar(builder.evm_mut().db_mut()).expect(
                        "DA footprint should always be available from the database post jovian",
                    ),
                );

            if info.is_tx_over_limits(
                tx_da_size,
                block_gas_limit,
                tx_da_limit,
                block_da_limit,
                tx.gas_limit().saturating_add(tx_payer_auth),
                da_footprint_gas_scalar,
            ) {
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

            let gas_output = match builder.execute_transaction(tx.clone()) {
                Ok(gas_output) => gas_output,
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
                })) => {
                    if error.is_nonce_too_low() {
                        trace!(target: "payload_builder", %error, ?tx, "skipping nonce too low transaction");
                    } else {
                        trace!(target: "payload_builder", %error, ?tx, "skipping invalid transaction and its descendants");
                        best_txs.mark_invalid(tx.signer(), tx.nonce());
                    }
                    continue;
                }
                Err(err) => {
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            info.cumulative_gas_used += gas_output.tx_gas_used();
            info.cumulative_da_bytes_used += tx_da_size;
            if let Some(usage) = pending_resource_usage
                && let Err(error) = usage.add_to(&mut info.resource_metering_usage)
            {
                warn!(
                    target: "payload_builder",
                    tx_hash = %tx_hash,
                    error = %error,
                    "resource metering cumulative usage overflowed"
                );
            }

            let miner_fee = tx
                .effective_tip_per_gas(base_fee)
                .expect("fee is always valid; execution succeeded");
            info.total_fees += U256::from(miner_fee) * U256::from(gas_output.tx_gas_used());
        }

        // A cancellation that raced the finalization break (or an empty iterator) must still
        // win, so re-check it before the finalized payload is assembled. Gated on Denim so
        // pre-Denim control flow is unchanged.
        if can_finalize_early && self.cancel.is_cancelled() {
            return Ok(Some(()));
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, mem::ManuallyDrop, sync::Arc};

    use alloy_consensus::{Header, SignableTransaction, TxEip1559};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Signature, StorageKey, TxKind, U256};
    use base_common_chains::BaseUpgrade;
    use base_common_consensus::{BasePrimitives, BaseTxEnvelope, Predeploys};
    use base_common_evm::BaseTime;
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use base_execution_evm::BaseEvmConfig;
    use base_execution_txpool::BasePooledTransaction;
    use reth_basic_payload_builder::{BuildOutcomeKind, PayloadConfig};
    use reth_chainspec::ChainSpec;
    use reth_ethereum_forks::ForkCondition;
    use reth_payload_builder::PayloadId;
    use reth_payload_util::{NoopPayloadTransactions, PayloadTransactions};
    use reth_primitives_traits::{Account, SealedHeader, SignedTransaction};
    use reth_provider::noop::NoopProvider;
    use reth_revm::{
        cancelled::CancelOnDrop, database::StateProviderDatabase, test_utils::StateProviderTest,
    };
    use reth_transaction_pool::PoolTransaction;
    use reth_trie_common::{HashedPostState, updates::TrieUpdates};
    use reth_trie_parallel::{
        error::StateRootTaskError,
        state_root_task::{
            PayloadStateRootHandle, StateRootComputeOutcome, StateRootSink, StateRootUpdateStream,
        },
    };
    use revm::state::EvmState;

    use super::{BasePayloadBuilderCtx, Builder, ExecutionInfo};
    use crate::{
        BasePayloadBuilderAttributes, config::BaseBuilderConfig,
        payload::EthPayloadBuilderAttributes,
    };

    #[derive(Debug)]
    struct TestStateRootSink {
        result: std::sync::mpsc::Sender<Result<StateRootComputeOutcome, StateRootTaskError>>,
    }

    impl StateRootSink for TestStateRootSink {
        fn on_state_update(&self, _state: EvmState) {}

        fn on_hashed_state_update(&self, _state: HashedPostState) {}

        fn on_updates_finished(&self) {
            _ = self.result.send(Ok(StateRootComputeOutcome {
                state_root: B256::repeat_byte(0x42),
                trie_updates: Arc::new(TrieUpdates::default()),
                hashed_state: Arc::new(HashedPostState::default()),
            }));
        }
    }

    fn state_root_handle() -> PayloadStateRootHandle {
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let hook = StateRootUpdateStream::new(Arc::new(TestStateRootSink { result: result_tx }))
            .into_state_hook();
        PayloadStateRootHandle::new("test", Some(hook), result_rx, None)
    }

    fn build_empty_payload(state_root_handle: PayloadStateRootHandle) -> B256 {
        let chain_spec = Arc::new(BaseChainSpec::from(ChainSpec::default()));
        let parent = Arc::new(SealedHeader::seal_slow(Header {
            gas_limit: 30_000_000,
            ..Default::default()
        }));
        let payload_id = PayloadId::new([0; 8]);
        let attributes = BasePayloadBuilderAttributes::<BaseTxEnvelope> {
            payload_attributes: EthPayloadBuilderAttributes {
                id: payload_id,
                parent: parent.hash(),
                timestamp: 2,
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
            no_tx_pool: true,
            gas_limit: Some(parent.gas_limit),
            ..Default::default()
        };
        let ctx = BasePayloadBuilderCtx {
            evm_config: BaseEvmConfig::<_, BasePrimitives>::base(Arc::clone(&chain_spec)),
            builder_config: BaseBuilderConfig::default(),
            chain_spec,
            config: PayloadConfig::new(parent, attributes, payload_id),
            cancel: Default::default(),
            best_payload: None,
        };
        let provider = NoopProvider::default();
        let builder = Builder::new(|_| NoopPayloadTransactions::<BasePooledTransaction>::default());
        let outcome = builder
            .build(StateProviderDatabase::new(&provider), &provider, Some(state_root_handle), ctx)
            .expect("empty payload must build");
        let BuildOutcomeKind::Freeze(payload) = outcome else {
            panic!("no-tx-pool payload must freeze")
        };
        payload.block().state_root
    }

    /// The block gas reservation must include EIP-8130 `payer_auth` on top of the
    /// declared `gas_limit`: a transaction that fits on `gas_limit` alone is still
    /// over the block limit once payer authentication is metered on top.
    #[test]
    fn is_tx_over_limits_reserves_eip8130_payer_auth() {
        let mut info = ExecutionInfo::new();
        info.cumulative_gas_used = 979_000;
        let block_gas_limit = 1_000_000;

        // gas_limit alone fits exactly (979_000 + 21_000 = 1_000_000).
        assert!(!info.is_tx_over_limits(0, block_gas_limit, None, None, 21_000, None));

        // payer_auth metered on top (reserved = 21_000 + 2_100) pushes over the block limit.
        assert!(info.is_tx_over_limits(0, block_gas_limit, None, None, 21_000 + 2_100, None));
    }

    #[test]
    fn parallel_state_root_is_used() {
        assert_eq!(build_empty_payload(state_root_handle()), B256::repeat_byte(0x42));
    }

    const DENIM_TIMESTAMP: u64 = 1;

    fn pool_payload_context(timestamp: u64) -> BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec> {
        let chain_spec = Arc::new(
            BaseChainSpecBuilder::base_mainnet()
                .with_fork(BaseUpgrade::Denim, ForkCondition::Timestamp(DENIM_TIMESTAMP))
                .build(),
        );
        let parent = Arc::new(SealedHeader::seal_slow(Header {
            gas_limit: 30_000_000,
            ..Default::default()
        }));
        let payload_id = PayloadId::new([0; 8]);
        let attributes = BasePayloadBuilderAttributes::<BaseTxEnvelope> {
            payload_attributes: EthPayloadBuilderAttributes {
                id: payload_id,
                parent: parent.hash(),
                timestamp,
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
            gas_limit: Some(parent.gas_limit),
            ..Default::default()
        };
        BasePayloadBuilderCtx {
            evm_config: BaseEvmConfig::<_, BasePrimitives>::base(Arc::clone(&chain_spec)),
            builder_config: BaseBuilderConfig::default(),
            chain_spec,
            config: PayloadConfig::new(parent, attributes, payload_id),
            cancel: Default::default(),
            best_payload: None,
        }
    }

    fn build_pool_payload<Txs>(
        ctx: BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>,
        transactions: Txs,
    ) -> BuildOutcomeKind<crate::BaseBuiltPayload<BasePrimitives>>
    where
        Txs: PayloadTransactions<Transaction = BasePooledTransaction> + Send + Sync,
    {
        let mut storage = HashMap::default();
        storage.insert(
            StorageKey::from(BaseTime::ADMIN_SLOT.to_be_bytes::<32>()),
            U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice()),
        );
        let mut provider = StateProviderTest::default();
        provider.insert_account(
            Predeploys::BASE_TIME,
            Account::default(),
            Some(BaseTime::proxy_bytecode()),
            storage,
        );
        provider.insert_account(
            pool_transaction(0).sender(),
            Account { balance: U256::MAX, ..Default::default() },
            None,
            HashMap::default(),
        );
        Builder::new(|_| transactions)
            .build(StateProviderDatabase::new(&provider), &provider, Some(state_root_handle()), ctx)
            .expect("payload must build")
    }

    fn pool_transaction(nonce: u64) -> BasePooledTransaction {
        let envelope = BaseTxEnvelope::Eip1559(
            TxEip1559 {
                chain_id: 8_453,
                nonce,
                gas_limit: 100_000,
                max_fee_per_gas: 2_000_000_000,
                max_priority_fee_per_gas: 1,
                to: TxKind::Call(Address::repeat_byte(0x11)),
                ..Default::default()
            }
            .into_signed(Signature::test_signature()),
        );
        let encoded_len = envelope.encode_2718_len();
        BasePooledTransaction::new(
            envelope.try_into_recovered().expect("test signature must recover"),
            encoded_len,
        )
    }

    struct FinalizeAfterFirstTransaction {
        transactions: std::vec::IntoIter<BasePooledTransaction>,
        calls: usize,
        // Models the resolver retaining its clone until the finalized payload is returned.
        cancel: ManuallyDrop<CancelOnDrop>,
    }

    impl PayloadTransactions for FinalizeAfterFirstTransaction {
        type Transaction = BasePooledTransaction;

        fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
            self.calls += 1;
            if self.calls == 2 {
                self.cancel.request_finalization();
            }
            self.transactions.next()
        }

        fn mark_invalid(&mut self, _sender: Address, _nonce: u64) {}
    }

    #[test]
    fn pre_denim_ignores_finalization_requests() {
        let ctx = pool_payload_context(DENIM_TIMESTAMP - 1);
        ctx.cancel.request_finalization();
        let transactions = FinalizeAfterFirstTransaction {
            transactions: vec![pool_transaction(0)].into_iter(),
            calls: 0,
            cancel: ManuallyDrop::new(ctx.cancel.clone()),
        };

        let BuildOutcomeKind::Better { payload } = build_pool_payload(ctx, transactions) else {
            panic!("pre-Denim payload must remain eligible for improvement")
        };
        assert_eq!(payload.block().body().transactions.len(), 1);
    }

    #[test]
    fn denim_finalization_preserves_completed_pool_transactions() {
        let ctx = pool_payload_context(DENIM_TIMESTAMP);
        let transactions = FinalizeAfterFirstTransaction {
            transactions: vec![pool_transaction(0), pool_transaction(1)].into_iter(),
            calls: 0,
            cancel: ManuallyDrop::new(ctx.cancel.clone()),
        };

        let BuildOutcomeKind::Freeze(payload) = build_pool_payload(ctx, transactions) else {
            panic!("Denim payload must freeze")
        };
        assert_eq!(payload.block().body().transactions.len(), 1);
    }

    #[test]
    fn cancellation_takes_precedence_over_finalization() {
        let ctx = pool_payload_context(DENIM_TIMESTAMP);
        ctx.cancel.request_finalization();
        drop(ctx.cancel.clone());

        assert!(matches!(
            build_pool_payload(ctx, NoopPayloadTransactions::<BasePooledTransaction>::default()),
            BuildOutcomeKind::Cancelled
        ));
    }
}
