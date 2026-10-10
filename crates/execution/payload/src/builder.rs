//! Base payload builder implementation.
use std::{
    marker::PhantomData,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::{BlockHeader, Transaction, Typed2718, transaction::TxHashRef};
use alloy_evm::{
    Evm as AlloyEvm,
    block::{CommitChanges, TxResult},
};
use alloy_primitives::{Address, B256, TxHash, U256};
use alloy_rpc_types_debug::ExecutionWitness;
use alloy_rpc_types_engine::PayloadId;
use base_common_chains::Upgrades;
use base_common_consensus::{BaseTransaction, DepositReceiptExt, Predeploys};
use base_common_evm::L1BlockInfo;
use base_execution_eip8130::{Eip8130GasSchedule, IntrinsicGas};
use base_execution_txpool::{
    BasePooledTx, GuardMetrics, ParkableTransactionPool, PredicateContext, ValidityPredicate,
    estimated_da_size::DataAvailabilitySized,
};
use base_observability_events::{
    GlobalTransactionEventWriter, TransactionEventProducer, TransactionEventType, transaction_event,
};
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, BuildOutcomeKind, MissingPayloadBehaviour, PayloadBuilder,
    PayloadConfig, is_better_payload,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, MIN_TRANSACTION_GAS};
use reth_evm::{
    BlockExecutorForEvm, ConfigureEvm, Database,
    execute::{
        BlockBuilder, BlockBuilderOutcome, BlockExecutionError, BlockExecutor, BlockValidationError,
    },
};
use reth_execution_types::BlockExecutionOutput;
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::{BuildNextEnv, BuiltPayloadExecutedBlock};
use reth_payload_util::{NoopPayloadTransactions, PayloadTransactions};
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
use tracing::{debug, debug_span, info, instrument, trace, warn};

use crate::{
    Attributes, BasePayloadBuilderAttributes, BlockDeferrals, BuilderMetrics,
    CoinbaseTipAffordability, InclusionTracker, MeteringProvider, ParkableBestPayloadTransactions,
    ParkablePayloadTransactions, ParkedPredicateIndex, PayloadPrimitives, PredicateLoadTracker,
    PredicateReadRecorder, RejectionCacheMetrics, StateChangeEffects, ValidityMetrics,
    ValidityPredicateEvaluation, config::BaseBuilderConfig, error::BasePayloadBuilderError,
    payload::BaseBuiltPayload,
};

macro_rules! emit_native_validity_event {
    ($ctx:expr, $event_type:expr, $tx_hash:expr, $attempt:expr, {
        $( $data_name:expr => $data_value:expr ),* $(,)?
    }) => {
        if GlobalTransactionEventWriter::get().is_some() {
            if let Err(error) = transaction_event!(
                producer: TransactionEventProducer::BaseBuilder,
                event_type: $event_type,
                tx_hash: $tx_hash,
                block_number: $ctx.parent().number().saturating_add(1),
                payload_id: $ctx.payload_id().to_string(),
                id: {
                    "validity_consideration_index" => $attempt,
                },
                data: {
                    "builder_mode" => "native",
                    "source_queue" => "txpool_best",
                    "parent_hash" => format!("{:#x}", $ctx.parent().hash()),
                    $( $data_name => $data_value ),*
                },
            ) {
                warn!(
                    target: "payload_builder",
                    error = %error,
                    event_type = %$event_type,
                    tx_hash = ?$tx_hash,
                    "failed to enqueue native builder validity transaction event"
                );
            }
        }
    };
}

/// Base payload builder
#[derive(Debug)]
pub struct BasePayloadBuilder<
    Pool,
    Client,
    Evm,
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
    /// Marker for the payload attributes type.
    _pd: PhantomData<Attrs>,
}

impl<Pool, Client, Evm, Attrs> Clone for BasePayloadBuilder<Pool, Client, Evm, Attrs>
where
    Pool: Clone,
    Client: Clone,
    Evm: ConfigureEvm,
{
    fn clone(&self) -> Self {
        Self {
            evm_config: self.evm_config.clone(),
            pool: self.pool.clone(),
            client: self.client.clone(),
            config: self.config.clone(),
            _pd: PhantomData,
        }
    }
}

impl<Pool, Client, Evm, Attrs> BasePayloadBuilder<Pool, Client, Evm, Attrs> {
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
        Self { pool, client, evm_config, config, _pd: PhantomData }
    }
}

impl<Pool, Client, Evm, N, Attrs> BasePayloadBuilder<Pool, Client, Evm, Attrs>
where
    Pool: TransactionPool<Transaction: BasePooledTx<Consensus = N::SignedTx>> + Clone,
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
        Txs: ParkablePayloadTransactions<
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

        let pool = self.pool.clone();
        let builder = Builder::new(best).with_permanent_eviction(move |hashes| {
            let _ = pool.remove_transactions(hashes);
        });

        let state_provider = crate::BuilderStateProvider::new(
            self.client.state_by_block_hash(ctx.parent().hash())?,
            execution_cache.map(|cache| cache.cache().clone()),
            self.config.state_provider_metrics,
        );
        let state = StateProviderDatabase::new(state_provider.provider());

        if ctx.attributes().no_tx_pool() {
            builder.build(state, state_provider.provider(), state_root_handle, ctx)
        } else {
            // sequencer mode we can reuse cachedreads from previous runs
            builder.build(
                cached_reads.as_db_mut(state),
                state_provider.provider(),
                state_root_handle,
                ctx,
            )
        }
        .map(|out| out.with_cached_reads(cached_reads))
    }

    /// Computes the witness for the payload.
    ///
    /// Stops early with [`BasePayloadBuilderError::Cancelled`] once `cancel` is cancelled.
    pub fn payload_witness(
        &self,
        parent: SealedHeader<N::BlockHeader>,
        attributes: Attrs::RpcPayloadAttributes,
        cancel: CancelOnDrop,
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
            cancel,
            best_payload: Default::default(),
        };

        let state_provider = self.client.state_by_block_hash(ctx.parent().hash())?;

        let builder = Builder::new(|_| NoopPayloadTransactions::<Pool::Transaction>::default());
        builder.witness(state_provider, &self.client, &ctx)
    }
}

/// Implementation of the [`PayloadBuilder`] trait for [`BasePayloadBuilder`].
impl<Pool, Client, Evm, N, Attrs> PayloadBuilder for BasePayloadBuilder<Pool, Client, Evm, Attrs>
where
    N: PayloadPrimitives,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: Upgrades> + BlockReader + Clone,
    Pool: ParkableTransactionPool<Transaction: BasePooledTx<Consensus = N::SignedTx>>,
    Evm: ConfigureEvm<
            Primitives = N,
            NextBlockEnvCtx: BuildNextEnv<Attrs, N::BlockHeader, Client::ChainSpec>,
        >,
    Attrs: Attributes<Transaction = N::SignedTx>,
{
    type Attributes = Attrs;
    type BuiltPayload = BaseBuiltPayload<N>;

    fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        self.build_payload(args, |attrs| {
            ParkableBestPayloadTransactions::new(
                self.pool.best_transactions_with_attributes_and_parking(attrs),
            )
        })
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
    /// Permanently removes mempool transactions that exceeded a per-transaction resource limit.
    #[debug(skip)]
    evict_permanently_rejected: Box<dyn FnOnce(Vec<TxHash>) + 'a>,
}

impl<'a, Txs> Builder<'a, Txs> {
    /// Creates a new [`Builder`].
    pub fn new(best: impl FnOnce(BestTransactionsAttributes) -> Txs + Send + Sync + 'a) -> Self {
        Self { best: Box::new(best), evict_permanently_rejected: Box::new(|_| {}) }
    }

    /// Sets the callback used to permanently remove mempool transactions that exceeded a
    /// per-transaction resource limit.
    pub fn with_permanent_eviction(mut self, evict: impl FnOnce(Vec<TxHash>) + 'a) -> Self {
        self.evict_permanently_rejected = Box::new(evict);
        self
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
        Txs: ParkablePayloadTransactions<
            Transaction: PoolTransaction<Consensus = N::SignedTx> + BasePooledTx,
        >,
        Attrs: Attributes<Transaction = N::SignedTx>,
    {
        let Self { best, evict_permanently_rejected } = self;
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
            let cancelled = ctx.execute_best_transactions(&mut info, &mut builder, best_txs)?;
            if !info.permanently_rejected_txs.is_empty() {
                let rejected = std::mem::take(&mut info.permanently_rejected_txs);
                let count = rejected.len();
                ctx.builder_config.rejection_cache.mark_rejected(&rejected);
                RejectionCacheMetrics::insertions().increment(count as u64);
                RejectionCacheMetrics::size()
                    .set(ctx.builder_config.rejection_cache.entry_count() as f64);
                MeteringProvider::remove(
                    ctx.builder_config.resource_metering.provider.as_ref(),
                    &rejected,
                );
                evict_permanently_rejected(rejected);
                info!(
                    target: "payload_builder",
                    count,
                    "evicted permanently rejected transactions from pool",
                );
            }
            if cancelled.is_some() {
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
        BuilderMetrics::record_inclusion(&info.inclusion);

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
        if ctx.cancel.is_cancelled() {
            return Err(PayloadBuilderError::other(BasePayloadBuilderError::Cancelled));
        }

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

        if ctx.cancel.is_cancelled() {
            return Err(PayloadBuilderError::other(BasePayloadBuilderError::Cancelled));
        }

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
    /// Inclusion and fee revenue from executed mempool transactions.
    pub inclusion: InclusionTracker,
    /// Cumulative resource-metering units for the current payload, aligned with the snapped schedule.
    pub resource_metering_usage: Vec<u128>,
    /// Transaction hashes that exceeded a per-transaction resource limit and should be
    /// permanently removed from the pool and recorded in the shared rejection cache.
    pub permanently_rejected_txs: Vec<TxHash>,
}

impl ExecutionInfo {
    /// Create a new instance with allocated slots.
    pub fn new() -> Self {
        Self {
            cumulative_gas_used: 0,
            cumulative_da_bytes_used: 0,
            total_fees: U256::ZERO,
            inclusion: InclusionTracker::default(),
            resource_metering_usage: Vec::new(),
            permanently_rejected_txs: Vec::new(),
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

/// A conservative lower bound on the gas a pool transaction reserves against the block gas
/// budget, not a minimum transaction cost.
///
/// A candidate reserves its `gas_limit` plus its EIP-8130 payer-authentication ceiling. For a
/// sponsored EIP-8130 transaction, `gas_limit` covers the sender's intrinsic gas and the AA
/// floor, and payer authentication is reserved on top, so the total reserved gas is at least
/// this bound. A standard transaction's intrinsic floor is [`MIN_TRANSACTION_GAS`] (21,000),
/// while an EIP-8130 transaction's is its `AA_BASE_COST` (15,000), so the conservative bound
/// across both families is the smaller value. Once the remaining block gas drops below this, no
/// candidate can fit and the scan can stop.
pub const MIN_TX_RESERVED_GAS: u64 = if Eip8130GasSchedule::AA_BASE_COST < MIN_TRANSACTION_GAS {
    Eip8130GasSchedule::AA_BASE_COST
} else {
    MIN_TRANSACTION_GAS
};

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

    /// Closes the iterator's current candidate.
    ///
    /// Replay-ID entries are independent, so they are committed rather than
    /// invalidating the sender's nonce lane.
    fn skip_current<B>(best_txs: &mut B, sender: Address, nonce: u64, replay_independent: bool)
    where
        B: ParkablePayloadTransactions,
        B::Transaction: PoolTransaction,
    {
        if replay_independent {
            best_txs.mark_current_committed();
        } else {
            best_txs.mark_invalid(sender, nonce);
        }
    }

    /// Journals `BUILDER_REJECTED` for a validity-gated candidate that passed its predicates but
    /// was skipped for another reason. With no `BUILDER_CONSIDERED`, this is the only builder
    /// event such a candidate gets.
    fn emit_validity_rejection(
        &self,
        tx_hash: TxHash,
        consideration_index: u64,
        reason: &'static str,
        detail: &'static str,
        permanent: bool,
    ) {
        emit_native_validity_event!(
            self,
            TransactionEventType::BuilderRejected,
            tx_hash,
            consideration_index,
            {
                "rejection_reason" => reason,
                "rejection_detail" => detail,
                "permanent" => permanent,
            }
        );
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
    #[instrument(
        skip_all,
        fields(phase = "sequencer_txs", block_number = tracing::field::Empty)
    )]
    pub fn execute_sequencer_transactions(
        &self,
        builder: &mut impl BlockBuilder<Primitives = Evm::Primitives>,
    ) -> Result<ExecutionInfo, PayloadBuilderError> {
        tracing::Span::current().record("block_number", self.parent().number().saturating_add(1));
        let mut info = ExecutionInfo::new();
        let no_tx_pool = self.attributes().no_tx_pool();
        let resource_metering = &self.builder_config.resource_metering;

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

            let mut pending_resource_usage = None;
            let tx_hash = *sequencer_tx.tx_hash();
            let gas_output = match builder.execute_transaction_with_commit_condition(
                sequencer_tx.clone(),
                |result| {
                    let result_and_state = result.result();
                    pending_resource_usage = resource_metering.unthrottled_usage(
                        &tx_hash,
                        result_and_state.result.tx_gas_used(),
                        &result_and_state.state,
                    );
                    CommitChanges::Yes
                },
            ) {
                Ok(Some(gas_output)) => gas_output,
                // Resource metering always returns [`CommitChanges::Yes`]. This arm
                // is the BlockBuilder commit-condition contract, not a metering skip.
                // Under `no_tx_pool` the attribute list is consensus input, so a
                // refused commit is fatal — same as `InvalidTx` in that mode.
                Ok(None) if !no_tx_pool => {
                    warn!(
                        target: "payload_builder",
                        tx_hash = %tx_hash,
                        "sequencer transaction commit was refused"
                    );
                    continue;
                }
                Ok(None) => {
                    return Err(PayloadBuilderError::other(
                        BasePayloadBuilderError::SequencerTransactionCommitRefused,
                    ));
                }
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
            if let Some(usage) = pending_resource_usage {
                resource_metering.apply_accounted_usage(
                    &tx_hash,
                    &usage,
                    &mut info.resource_metering_usage,
                );
            }
        }
        Ok(info)
    }

    /// Executes the given best transactions and updates the execution info.
    ///
    /// Transaction-scope resource-throttling excludes are recorded on
    /// [`ExecutionInfo::permanently_rejected_txs`] for the payload job to remove from the
    /// pool and insert into [`crate::config::BaseBuilderConfig::rejection_cache`] after this scan.
    /// Later jobs sharing that cache skip the hash even if the pool has it again.
    /// Nonce-lane descendants are skipped for this scan via `mark_invalid`;
    /// skipping those descendants across later jobs is Flashblocks-only.
    /// Block-scope excludes only skip the current iterator.
    ///
    /// Returns `Ok(Some(()))` if the job was cancelled.
    #[instrument(skip_all, fields(phase = "mempool_txs"))]
    pub fn execute_best_transactions<Builder>(
        &self,
        info: &mut ExecutionInfo,
        builder: &mut Builder,
        mut best_txs: impl ParkablePayloadTransactions<
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
        let block_number =
            builder.evm_mut().block().number().try_into().expect("block number must fit in u64");
        let predicate_context = PredicateContext { block_number, flashblock_index: 0 };
        let mut predicate_index =
            ParkedPredicateIndex::new(self.builder_config.predicate_bucket_ordered_threshold);
        let mut predicate_loads = PredicateLoadTracker::default();
        let mut predicate_eval_duration = None;
        let mut predicate_bucket_wakeups = 0;
        let mut validity_consideration_index = 0_u64;
        // A blocked transaction is parked again each time a promotion re-yields it. Post-Denim
        // the payload is built once, so this covers the whole block; pre-Denim rebuilds start a
        // fresh scan and report deferrals again.
        let mut deferrals = BlockDeferrals::default();
        let mut validity_candidates_evaluated = 0_u64;
        let mut validity_candidates_deferred = 0_u64;
        let mut predicate_eval_cutoff_hit = false;

        let block_timestamp = self.attributes().timestamp();
        let can_finalize_early = self.is_denim_active();
        let resource_metering = &self.builder_config.resource_metering;
        let mut resource_throttled = 0u64;
        loop {
            // Cancellation is observed before the gas guard stops the scan, so a cancelled job
            // still returns `Ok(Some(()))` when the block gas is already exhausted. The guard
            // stays ahead of `next` so an exhausted block never reads the pool.
            if self.cancel.is_cancelled() {
                return Ok(Some(()));
            }
            if block_gas_limit.saturating_sub(info.cumulative_gas_used) < MIN_TX_RESERVED_GAS {
                break;
            }
            let Some(tx) = best_txs.next(()) else {
                break;
            };
            if can_finalize_early && self.cancel.is_finalization_requested() {
                break;
            }

            let tx_hash = *tx.hash();
            let replay_independent = tx.eip8130_replay_id().is_some();
            if self.builder_config.rejection_cache.is_rejected(&tx_hash) {
                RejectionCacheMetrics::hits().increment(1);
                RejectionCacheMetrics::size()
                    .set(self.builder_config.rejection_cache.entry_count() as f64);
                trace!(
                    target: "payload_builder",
                    tx_hash = %tx_hash,
                    "skipping previously rejected transaction"
                );
                Self::skip_current(&mut best_txs, tx.sender(), tx.nonce(), replay_independent);
                continue;
            }

            let has_validity_predicates = !tx.validity_predicates().is_empty();
            let coinbase_tip = tx.coinbase_tip();
            let has_coinbase_tip = coinbase_tip.is_some();
            // Every validity candidate ends with a decision event, so there is no separate
            // `BUILDER_CONSIDERED`.
            if has_validity_predicates {
                validity_consideration_index += 1;
            }
            if tx
                .validity_predicates()
                .iter()
                .any(|predicate| matches!(predicate, ValidityPredicate::FlashblockIndex { .. }))
            {
                ValidityMetrics::validity_predicate_evaluations_total("unsupported").increment(1);
                emit_native_validity_event!(
                    self,
                    TransactionEventType::BuilderRejected,
                    tx_hash,
                    validity_consideration_index,
                    {
                        "rejection_reason" => "unsupported_flashblock_index_predicate",
                        "rejection_detail" =>
                            "flashblock-index predicates are unsupported by the native builder",
                        "permanent" => true,
                    }
                );
                trace!(
                    target: "payload_builder",
                    tx_hash = ?tx_hash,
                    "skipping transaction with unsupported flashblock-index predicate"
                );
                Self::skip_current(&mut best_txs, tx.sender(), tx.nonce(), replay_independent);
                continue;
            }

            if has_validity_predicates
                && predicate_eval_duration.is_some_and(|duration| {
                    duration >= self.builder_config.predicate_eval_hard_cutoff
                })
            {
                ValidityMetrics::validity_predicate_evaluations_total("budget_exhausted")
                    .increment(1);
                validity_candidates_deferred += 1;
                predicate_eval_cutoff_hit = true;
                trace!(
                    target: "payload_builder",
                    tx_hash = ?tx_hash,
                    "deferring validity-gated transaction: predicate evaluation budget exhausted"
                );
                best_txs.park_current();
                if deferrals.record(tx_hash, "predicate_eval_budget_exhausted") {
                    emit_native_validity_event!(
                        self,
                        TransactionEventType::BuilderDeferred,
                        tx_hash,
                        validity_consideration_index,
                        {
                            "defer_reason" => "predicate_eval_budget_exhausted",
                            "defer_detail" => "validity-predicate evaluation time budget exhausted for this payload build",
                        }
                    );
                }
                continue;
            }

            if has_validity_predicates {
                validity_candidates_evaluated += 1;
                let evaluation_start = Instant::now();
                let evaluation = {
                    let mut recorder = PredicateReadRecorder::new(
                        builder.evm_mut().db_mut(),
                        &mut predicate_loads,
                    );
                    ValidityPredicateEvaluation::evaluate(
                        tx.validity_predicates(),
                        &mut recorder,
                        &predicate_context,
                    )
                };
                *predicate_eval_duration.get_or_insert(Duration::ZERO) +=
                    evaluation_start.elapsed();
                match evaluation {
                    Ok(ValidityPredicateEvaluation::Matched) => {
                        ValidityMetrics::validity_predicate_evaluations_total("matched")
                            .increment(1);
                    }
                    Ok(ValidityPredicateEvaluation::Unsatisfied { expired: true, .. }) => {
                        ValidityMetrics::validity_predicate_evaluations_total("expired")
                            .increment(1);
                        emit_native_validity_event!(
                            self,
                            TransactionEventType::BuilderExpired,
                            tx_hash,
                            validity_consideration_index,
                            {
                                "expire_reason" => "validity_predicate_expired",
                                "expire_detail" => "a validity predicate can no longer be satisfied at or after the current block",
                            }
                        );
                        trace!(
                            target: "payload_builder",
                            tx_hash = ?tx_hash,
                            "skipping transaction with expired validity predicate"
                        );
                        Self::skip_current(
                            &mut best_txs,
                            tx.sender(),
                            tx.nonce(),
                            replay_independent,
                        );
                        continue;
                    }
                    Ok(ValidityPredicateEvaluation::Unsatisfied {
                        blocker,
                        blocker_index,
                        expired: false,
                    }) => {
                        ValidityMetrics::validity_predicate_evaluations_total("not_satisfied")
                            .increment(1);
                        trace!(
                            target: "payload_builder",
                            tx_hash = ?tx_hash,
                            ?blocker,
                            "parking transaction with unsatisfied validity predicate"
                        );
                        best_txs.park_current();
                        if deferrals.record(tx_hash, "validity_predicate_not_satisfied") {
                            emit_native_validity_event!(
                                self,
                                TransactionEventType::BuilderDeferred,
                                tx_hash,
                                validity_consideration_index,
                                {
                                    "defer_reason" => "validity_predicate_not_satisfied",
                                    "defer_detail" => "a validity predicate is not satisfied by the current build state",
                                }
                            );
                        }
                        let predicate = tx.validity_predicates()[blocker_index].clone();
                        predicate_index.park(tx_hash, tx, predicate);
                        continue;
                    }
                    Err(error) => {
                        ValidityMetrics::validity_predicate_evaluations_total("read_error")
                            .increment(1);
                        emit_native_validity_event!(
                            self,
                            TransactionEventType::BuilderRejected,
                            tx_hash,
                            validity_consideration_index,
                            {
                                "rejection_reason" => "validity_predicate_read_failed",
                                "rejection_detail" => "failed to read state required by a validity predicate",
                                "permanent" => false,
                            }
                        );
                        warn!(
                            target: "payload_builder",
                            tx_hash = ?tx_hash,
                            error = ?error,
                            "failed to read validity predicate state"
                        );
                        Self::skip_current(
                            &mut best_txs,
                            tx.sender(),
                            tx.nonce(),
                            replay_independent,
                        );
                        continue;
                    }
                }
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
                if has_validity_predicates {
                    self.emit_validity_rejection(
                        tx_hash,
                        validity_consideration_index,
                        "manifest_precheck_stale",
                        stale.cause(),
                        false,
                    );
                }
                Self::skip_current(&mut best_txs, tx.sender(), tx.nonce(), replay_independent);
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
                        if has_validity_predicates {
                            self.emit_validity_rejection(
                                tx_hash,
                                validity_consideration_index,
                                "unschedulable_payer_authenticator",
                                "EIP-8130 payer authenticator cannot be scheduled against the gas budget",
                                false,
                            );
                        }
                        Self::skip_current(
                            &mut best_txs,
                            tx.sender(),
                            tx.nonce(),
                            replay_independent,
                        );
                        continue;
                    }
                },
                None => 0,
            };

            if CoinbaseTipAffordability::unaffordable(
                &tx,
                tx_payer_auth,
                builder.evm_mut().db_mut(),
            ) {
                trace!(
                    target: "payload_builder",
                    tx_hash = ?tx.hash(),
                    "skipping transaction unable to pay gas plus declared coinbase tip"
                );
                if has_validity_predicates {
                    self.emit_validity_rejection(
                        tx_hash,
                        validity_consideration_index,
                        "unaffordable_coinbase_tip",
                        "sender and gas payer cannot cover worst-case gas plus the declared coinbase tip",
                        false,
                    );
                }
                Self::skip_current(&mut best_txs, tx.sender(), tx.nonce(), replay_independent);
                continue;
            }

            let (simulated, admission) =
                resource_metering.check_simulated_usage(&tx_hash, &info.resource_metering_usage);
            if admission.should_exclude() {
                resource_throttled += 1;
                // Transaction-scope excludes cannot fit any block and are collected for
                // permanent pool eviction after this scan. Block-scope excludes only skip
                // the current iterator.
                if admission.is_permanent() {
                    info.permanently_rejected_txs.push(tx_hash);
                }
                trace!(
                    target: "payload_builder",
                    tx_hash = %tx_hash,
                    "skipping transaction excluded by simulated resource metering"
                );
                if has_validity_predicates {
                    self.emit_validity_rejection(
                        tx_hash,
                        validity_consideration_index,
                        "resource_metering_excluded",
                        "simulated resource usage exceeds the resource metering budget",
                        admission.is_permanent(),
                    );
                }
                Self::skip_current(&mut best_txs, tx.sender(), tx.nonce(), replay_independent);
                continue;
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
                if has_validity_predicates {
                    self.emit_validity_rejection(
                        tx_hash,
                        validity_consideration_index,
                        "block_limits_exceeded",
                        "transaction does not fit the remaining block gas or DA budget",
                        false,
                    );
                }
                Self::skip_current(&mut best_txs, tx.signer(), tx.nonce(), replay_independent);
                continue;
            }

            // A sequencer's block should never contain blob or deposit transactions from the pool.
            if tx.is_eip4844() || tx.is_deposit() {
                if has_validity_predicates {
                    self.emit_validity_rejection(
                        tx_hash,
                        validity_consideration_index,
                        "sequencer_transaction",
                        "blob and deposit transactions are not built from the pool",
                        true,
                    );
                }
                Self::skip_current(&mut best_txs, tx.signer(), tx.nonce(), replay_independent);
                continue;
            }

            if self.cancel.is_cancelled() {
                return Ok(Some(()));
            }

            let mut state_change_effects = StateChangeEffects::default();
            let mut pending_resource_usage = None;
            let mut executed_decision = None;
            let gas_output = match builder.execute_transaction_with_commit_condition(
                tx.clone(),
                |result| {
                    let result_and_state = result.result();
                    let decision = resource_metering.check_executed_usage(
                        &tx_hash,
                        result_and_state.result.tx_gas_used(),
                        &result_and_state.state,
                        simulated.as_ref(),
                        &info.resource_metering_usage,
                    );
                    if decision.should_exclude() {
                        executed_decision = Some(decision);
                        CommitChanges::No
                    } else {
                        pending_resource_usage = decision.committed_usage();
                        if !predicate_index.is_empty() {
                            state_change_effects =
                                predicate_index.affected_by_state(&result_and_state.state);
                            predicate_bucket_wakeups += state_change_effects.woken_buckets as u64;
                        }
                        CommitChanges::Yes
                    }
                },
            ) {
                Ok(Some(gas_output)) => gas_output,
                Ok(None) => {
                    resource_throttled += 1;
                    if executed_decision.as_ref().is_some_and(|decision| decision.is_permanent()) {
                        info.permanently_rejected_txs.push(tx_hash);
                    }
                    trace!(
                        target: "payload_builder",
                        tx_hash = %tx_hash,
                        "skipping transaction excluded by resource metering"
                    );
                    if has_validity_predicates {
                        self.emit_validity_rejection(
                            tx_hash,
                            validity_consideration_index,
                            "resource_metering_excluded",
                            "executed resource usage exceeds the resource metering budget",
                            executed_decision
                                .as_ref()
                                .is_some_and(|decision| decision.is_permanent()),
                        );
                    }
                    Self::skip_current(&mut best_txs, tx.signer(), tx.nonce(), replay_independent);
                    continue;
                }
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
                })) => {
                    if error.is_nonce_too_low() {
                        trace!(target: "payload_builder", %error, ?tx, "skipping nonce too low transaction");
                        if has_validity_predicates {
                            self.emit_validity_rejection(
                                tx_hash,
                                validity_consideration_index,
                                "nonce_too_low",
                                "transaction nonce is below the sender's current nonce",
                                true,
                            );
                        }
                        best_txs.mark_current_committed();
                    } else {
                        trace!(target: "payload_builder", %error, ?tx, "skipping invalid transaction and its descendants");
                        if has_validity_predicates {
                            self.emit_validity_rejection(
                                tx_hash,
                                validity_consideration_index,
                                "invalid_transaction",
                                "transaction failed EVM validation",
                                false,
                            );
                        }
                        Self::skip_current(
                            &mut best_txs,
                            tx.signer(),
                            tx.nonce(),
                            replay_independent,
                        );
                    }
                    continue;
                }
                Err(err) => {
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            info.cumulative_gas_used += gas_output.tx_gas_used();
            info.cumulative_da_bytes_used += tx_da_size;
            if let Some(usage) = pending_resource_usage {
                resource_metering.apply_accounted_usage(
                    &tx_hash,
                    &usage,
                    &mut info.resource_metering_usage,
                );
            }

            best_txs.mark_current_committed();
            let predicates_need_rescan = !state_change_effects.affected_transactions.is_empty();
            let predicate_rescan_start = Instant::now();
            for (rescanned, parked_hash) in
                state_change_effects.affected_transactions.iter().copied().enumerate()
            {
                if predicate_eval_duration.is_some_and(|duration| {
                    duration >= self.builder_config.predicate_eval_hard_cutoff
                }) {
                    let remaining = state_change_effects.affected_transactions.len() - rescanned;
                    ValidityMetrics::validity_predicate_evaluations_total(
                        "rescan_budget_exhausted",
                    )
                    .increment(remaining as u64);
                    predicate_eval_cutoff_hit = true;
                    break;
                }
                let Some(parked_transaction) = predicate_index.transaction(parked_hash) else {
                    continue;
                };
                let evaluation_start = Instant::now();
                let evaluation = {
                    let mut recorder = PredicateReadRecorder::new(
                        builder.evm_mut().db_mut(),
                        &mut predicate_loads,
                    );
                    ValidityPredicateEvaluation::evaluate(
                        parked_transaction.validity_predicates(),
                        &mut recorder,
                        &predicate_context,
                    )
                };
                *predicate_eval_duration.get_or_insert(Duration::ZERO) +=
                    evaluation_start.elapsed();
                match evaluation {
                    Ok(ValidityPredicateEvaluation::Matched) => {
                        ValidityMetrics::validity_predicate_evaluations_total("rescan_matched")
                            .increment(1);
                        predicate_index.remove(parked_hash);
                        best_txs.promote(parked_hash);
                    }
                    Ok(ValidityPredicateEvaluation::Unsatisfied { blocker_index, .. }) => {
                        ValidityMetrics::validity_predicate_evaluations_total(
                            "rescan_not_satisfied",
                        )
                        .increment(1);
                        let predicate =
                            parked_transaction.validity_predicates()[blocker_index].clone();
                        predicate_index.reindex(parked_hash, predicate);
                    }
                    Err(error) => {
                        ValidityMetrics::validity_predicate_evaluations_total("rescan_read_error")
                            .increment(1);
                        warn!(
                            target: "payload_builder",
                            tx_hash = ?parked_hash,
                            error = ?error,
                            "failed to re-read validity predicate state"
                        );
                        predicate_index.remove(parked_hash);
                        best_txs.discard_parked(parked_hash);
                    }
                }
            }
            if predicates_need_rescan {
                ValidityMetrics::validity_predicate_rescan_duration()
                    .record(predicate_rescan_start.elapsed().as_secs_f64());
            }

            let miner_fee = tx
                .effective_tip_per_gas(base_fee)
                .expect("fee is always valid; execution succeeded");
            let gas_used = gas_output.tx_gas_used();
            info.total_fees += U256::from(miner_fee) * U256::from(gas_used);
            // The tip is a phase-0 transfer, so it is revenue only if that phase
            // committed; a reverted or policy-blocked phase pays nothing.
            let coinbase_tip = coinbase_tip.filter(|_| {
                builder
                    .executor()
                    .receipts()
                    .last()
                    .and_then(DepositReceiptExt::as_eip8130_receipt)
                    .is_some_and(|receipt| receipt.phase_committed(0))
            });
            info.inclusion.record(
                has_validity_predicates,
                gas_used,
                miner_fee,
                base_fee,
                coinbase_tip.unwrap_or_default(),
            );
            BuilderMetrics::record_tip_per_gas(
                has_validity_predicates,
                has_coinbase_tip,
                miner_fee as f64,
            );
            if has_validity_predicates {
                emit_native_validity_event!(
                    self,
                    TransactionEventType::BuilderAccepted,
                    tx_hash,
                    validity_consideration_index,
                    {
                        "gas_used" => gas_output.tx_gas_used(),
                    }
                );
            }
        }

        if let Some(predicate_eval_duration) = predicate_eval_duration {
            ValidityMetrics::record_predicate_eval_duration(predicate_eval_duration);
        }
        ValidityMetrics::record_predicate_evaluation_coverage(
            validity_candidates_evaluated,
            validity_candidates_deferred,
            predicate_eval_cutoff_hit,
        );
        ValidityMetrics::record_predicate_loads(&predicate_loads);
        ValidityMetrics::record_predicate_index_diagnostics(
            predicate_bucket_wakeups,
            &predicate_index,
        );
        if resource_throttled > 0 {
            info!(
                target: "payload_builder",
                throttled = resource_throttled,
                permanently_rejected = info.permanently_rejected_txs.len(),
                "resource metering throttled transactions during payload scan"
            );
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
    use std::{
        collections::{HashMap, VecDeque},
        mem::ManuallyDrop,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy_consensus::{Header, SignableTransaction, TxEip1559};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_evm::Evm;
    use alloy_primitives::{Address, B256, Signature, StorageKey, TxHash, TxKind, U256};
    use base_bundles::{MeterBundleResponse, OpcodeGas, TransactionResult};
    use base_common_chains::BaseUpgrade;
    use base_common_consensus::{BasePrimitives, BaseTxEnvelope, Predeploys};
    use base_common_evm::BaseTime;
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use base_execution_evm::BaseEvmConfig;
    use base_execution_txpool::{BasePooledTransaction, ValidityOperator, ValidityPredicate};
    use base_observability_events::{TransactionEventCapture, TransactionEventType};
    use reth_basic_payload_builder::{BuildOutcomeKind, PayloadConfig};
    use reth_chainspec::ChainSpec;
    use reth_ethereum_forks::ForkCondition;
    use reth_evm::execute::BlockBuilder;
    use reth_payload_builder::PayloadId;
    use reth_payload_util::{NoopPayloadTransactions, PayloadTransactions};
    use reth_primitives_traits::{Account, SealedHeader, SignedTransaction, WithEncoded};
    use reth_provider::noop::NoopProvider;
    use reth_revm::{
        cancelled::CancelOnDrop, database::StateProviderDatabase, db::State,
        test_utils::StateProviderTest,
    };
    use reth_transaction_pool::PoolTransaction;
    use reth_trie_common::{HashedPostState, updates::TrieUpdates};
    use reth_trie_parallel::{
        error::StateRootTaskError,
        state_root_task::{
            PayloadStateRootHandle, StateRootComputeOutcome, StateRootSink, StateRootUpdateStream,
        },
    };
    use revm::{Database, context::Block, state::EvmState};

    use super::{BasePayloadBuilderCtx, Builder, ExecutionInfo, MIN_TX_RESERVED_GAS};
    use crate::{
        BasePayloadBuilderAttributes, MeteringProvider, NoopMeteringProvider,
        ParkablePayloadTransactions, ResourceMeteringConfig, ResourceMeteringDimension,
        ResourceMeteringOperation, ResourceMeteringSchedule, SharedMeteringProvider,
        config::BaseBuilderConfig, payload::EthPayloadBuilderAttributes,
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
        Txs: ParkablePayloadTransactions<Transaction = BasePooledTransaction> + Send + Sync,
    {
        let funded_sender = pool_transaction(0).sender();
        build_parkable_pool_payload(ctx, transactions, &[funded_sender])
    }

    fn build_parkable_pool_payload<Txs>(
        ctx: BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>,
        transactions: Txs,
        funded_senders: &[Address],
    ) -> BuildOutcomeKind<crate::BaseBuiltPayload<BasePrimitives>>
    where
        Txs: ParkablePayloadTransactions<Transaction = BasePooledTransaction> + Send + Sync,
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
        for sender in funded_senders {
            provider.insert_account(
                *sender,
                Account { balance: U256::MAX, ..Default::default() },
                None,
                HashMap::default(),
            );
        }
        Builder::new(|_| transactions)
            .build(StateProviderDatabase::new(&provider), &provider, Some(state_root_handle()), ctx)
            .expect("payload must build")
    }

    fn build_pool_payload_with<Txs>(
        ctx: BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>,
        transactions: Txs,
        evict: impl FnOnce(Vec<TxHash>),
    ) -> BuildOutcomeKind<crate::BaseBuiltPayload<BasePrimitives>>
    where
        Txs: ParkablePayloadTransactions<Transaction = BasePooledTransaction> + Send + Sync,
    {
        let funded_sender = pool_transaction(0).sender();
        build_parkable_pool_payload_with(ctx, transactions, &[funded_sender], evict)
    }

    fn build_parkable_pool_payload_with<Txs>(
        ctx: BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>,
        transactions: Txs,
        funded_senders: &[Address],
        evict: impl FnOnce(Vec<TxHash>),
    ) -> BuildOutcomeKind<crate::BaseBuiltPayload<BasePrimitives>>
    where
        Txs: ParkablePayloadTransactions<Transaction = BasePooledTransaction> + Send + Sync,
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
        for sender in funded_senders {
            provider.insert_account(
                *sender,
                Account { balance: U256::MAX, ..Default::default() },
                None,
                HashMap::default(),
            );
        }
        Builder::new(|_| transactions)
            .with_permanent_eviction(evict)
            .build(StateProviderDatabase::new(&provider), &provider, Some(state_root_handle()), ctx)
            .expect("payload must build")
    }

    fn pool_transaction(nonce: u64) -> BasePooledTransaction {
        pool_transaction_to(nonce, Address::repeat_byte(0x11), U256::ZERO)
    }

    fn pool_transaction_to(nonce: u64, to: Address, value: U256) -> BasePooledTransaction {
        let envelope = BaseTxEnvelope::Eip1559(
            TxEip1559 {
                chain_id: 8_453,
                nonce,
                gas_limit: 100_000,
                max_fee_per_gas: 2_000_000_000,
                max_priority_fee_per_gas: 1,
                to: TxKind::Call(to),
                value,
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

    /// Scripted iterator is required here because predicate promotion mutates which transaction
    /// `next` returns while the build is in flight, which a static mock cannot express.
    struct TestParkableTransactions {
        queued: VecDeque<BasePooledTransaction>,
        ready: VecDeque<BasePooledTransaction>,
        parked: HashMap<B256, BasePooledTransaction>,
        current: Option<BasePooledTransaction>,
        /// Whether promoted transactions are yielded before the remaining queued ones, as when
        /// they outbid them.
        promoted_first: bool,
        invalid: Arc<Mutex<Vec<(Address, u64)>>>,
        /// Number of `next` calls, so a test can assert the gas guard did not read the pool.
        next_calls: Arc<Mutex<usize>>,
    }

    impl TestParkableTransactions {
        fn new(transactions: Vec<BasePooledTransaction>) -> Self {
            Self::recording(transactions, Arc::default())
        }

        /// Records every `(sender, nonce)` the builder marks invalid into `invalid`.
        fn recording(
            transactions: Vec<BasePooledTransaction>,
            invalid: Arc<Mutex<Vec<(Address, u64)>>>,
        ) -> Self {
            Self {
                queued: transactions.into(),
                ready: VecDeque::new(),
                parked: HashMap::default(),
                current: None,
                promoted_first: true,
                invalid,
                next_calls: Arc::default(),
            }
        }

        /// Yields promoted transactions only after every queued one, so queued transactions can
        /// change state between a promotion and the re-yield.
        fn with_promoted_last(transactions: Vec<BasePooledTransaction>) -> Self {
            Self { promoted_first: false, ..Self::new(transactions) }
        }

        /// Records every `next` call into `next_calls` and every invalidated `(sender, nonce)`
        /// into `invalid`, so a test can assert whether the gas guard read the pool at all.
        fn counting(
            transactions: Vec<BasePooledTransaction>,
            invalid: Arc<Mutex<Vec<(Address, u64)>>>,
            next_calls: Arc<Mutex<usize>>,
        ) -> Self {
            Self { next_calls, ..Self::recording(transactions, invalid) }
        }
    }

    impl PayloadTransactions for TestParkableTransactions {
        type Transaction = BasePooledTransaction;

        fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
            *self.next_calls.lock().unwrap() += 1;
            assert!(self.current.is_none(), "current transaction was not lifecycle-managed");
            let transaction = if self.promoted_first {
                self.ready.pop_front().or_else(|| self.queued.pop_front())
            } else {
                self.queued.pop_front().or_else(|| self.ready.pop_front())
            }?;
            self.current = Some(transaction.clone());
            Some(transaction)
        }

        fn mark_invalid(&mut self, sender: Address, nonce: u64) {
            self.current = None;
            self.invalid.lock().unwrap().push((sender, nonce));
        }
    }

    impl ParkablePayloadTransactions for TestParkableTransactions {
        fn park_current(&mut self) {
            if let Some(transaction) = self.current.take() {
                self.parked.insert(*transaction.hash(), transaction);
            }
        }

        fn mark_current_committed(&mut self) {
            self.current = None;
        }

        fn promote(&mut self, transaction_hash: B256) -> bool {
            let Some(transaction) = self.parked.remove(&transaction_hash) else {
                return false;
            };
            self.ready.push_back(transaction);
            true
        }

        fn discard_parked(&mut self, transaction_hash: B256) -> bool {
            self.parked.remove(&transaction_hash).is_some()
        }
    }

    struct FinalizeAfterFirstTransaction {
        transactions: TestParkableTransactions,
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
            self.transactions.next(())
        }

        fn mark_invalid(&mut self, sender: Address, nonce: u64) {
            self.transactions.mark_invalid(sender, nonce);
        }
    }

    impl ParkablePayloadTransactions for FinalizeAfterFirstTransaction {
        fn park_current(&mut self) {
            self.transactions.park_current();
        }

        fn mark_current_committed(&mut self) {
            self.transactions.mark_current_committed();
        }

        fn promote(&mut self, transaction_hash: B256) -> bool {
            self.transactions.promote(transaction_hash)
        }

        fn discard_parked(&mut self, transaction_hash: B256) -> bool {
            self.transactions.discard_parked(transaction_hash)
        }
    }

    #[test]
    fn pre_denim_ignores_finalization_requests() {
        let ctx = pool_payload_context(DENIM_TIMESTAMP - 1);
        ctx.cancel.request_finalization();
        let transactions = FinalizeAfterFirstTransaction {
            transactions: TestParkableTransactions::new(vec![pool_transaction(0)]),
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
            transactions: TestParkableTransactions::new(vec![
                pool_transaction(0),
                pool_transaction(1),
            ]),
            calls: 0,
            cancel: ManuallyDrop::new(ctx.cancel.clone()),
        };

        let BuildOutcomeKind::Freeze(payload) = build_pool_payload(ctx, transactions) else {
            panic!("Denim payload must freeze")
        };
        assert_eq!(payload.block().body().transactions.len(), 1);
    }

    /// Once the remaining block gas is below the smallest amount any candidate can reserve, the
    /// scan must stop before touching the pool: every candidate would be rejected, and the scan
    /// itself is the cost the guard removes.
    #[test]
    fn native_scan_stops_before_the_pool_once_gas_cannot_fit_any_transaction() {
        let ctx = pool_payload_context(DENIM_TIMESTAMP - 1);
        let provider = test_state_provider();
        let mut db = State::builder()
            .with_database(StateProviderDatabase::new(&provider))
            .with_bundle_update()
            .build();
        db.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("L1 block info must load");
        let mut builder = ctx.block_builder(&mut db).expect("block builder");
        builder.apply_pre_execution_changes().expect("pre-execution changes");
        let mut info = ctx.execute_sequencer_transactions(&mut builder).expect("sequencer");
        let block_gas_limit = builder.evm_mut().block().gas_limit();
        info.cumulative_gas_used = block_gas_limit - MIN_TX_RESERVED_GAS + 1;

        let invalid = Arc::new(Mutex::new(Vec::new()));
        let next_calls = Arc::new(Mutex::new(0));
        let transactions = TestParkableTransactions::counting(
            vec![pool_transaction(0)],
            Arc::clone(&invalid),
            Arc::clone(&next_calls),
        );

        let cancelled = ctx
            .execute_best_transactions(&mut info, &mut builder, transactions)
            .expect("mempool scan");

        assert!(cancelled.is_none());
        assert_eq!(
            *next_calls.lock().unwrap(),
            0,
            "the scan must not read the pool when no tx can fit"
        );
        assert!(invalid.lock().unwrap().is_empty());
        assert_eq!(info.cumulative_gas_used, block_gas_limit - MIN_TX_RESERVED_GAS + 1);
    }

    /// The guard must not stop a scan that still has room for the smallest reservable
    /// transaction: at exactly `MIN_TX_RESERVED_GAS` remaining the loop still considers a
    /// candidate.
    #[test]
    fn native_scan_considers_candidates_while_the_smallest_transaction_fits() {
        let ctx = pool_payload_context(DENIM_TIMESTAMP - 1);
        let provider = test_state_provider();
        let mut db = State::builder()
            .with_database(StateProviderDatabase::new(&provider))
            .with_bundle_update()
            .build();
        db.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("L1 block info must load");
        let mut builder = ctx.block_builder(&mut db).expect("block builder");
        builder.apply_pre_execution_changes().expect("pre-execution changes");
        let mut info = ctx.execute_sequencer_transactions(&mut builder).expect("sequencer");
        let block_gas_limit = builder.evm_mut().block().gas_limit();
        info.cumulative_gas_used = block_gas_limit - MIN_TX_RESERVED_GAS;

        let invalid = Arc::new(Mutex::new(Vec::new()));
        let next_calls = Arc::new(Mutex::new(0));
        let transactions = TestParkableTransactions::counting(
            vec![pool_transaction(0)],
            Arc::clone(&invalid),
            Arc::clone(&next_calls),
        );

        let cancelled = ctx
            .execute_best_transactions(&mut info, &mut builder, transactions)
            .expect("mempool scan");

        assert!(cancelled.is_none());
        assert!(
            *next_calls.lock().unwrap() >= 1,
            "the scan must read the pool while a tx could fit"
        );
        // The candidate's 21,000-gas limit does not fit the remaining gas, so it is rejected by
        // the gas limit rather than silently skipped.
        assert_eq!(invalid.lock().unwrap().len(), 1);
        assert_eq!(info.cumulative_gas_used, block_gas_limit - MIN_TX_RESERVED_GAS);
    }

    /// A cancelled job must not be finalized even when the block gas is already exhausted, so the
    /// cancellation check runs before the gas guard stops the scan.
    #[test]
    fn native_cancellation_wins_over_the_gas_guard() {
        let ctx = pool_payload_context(DENIM_TIMESTAMP - 1);
        let provider = test_state_provider();
        let mut db = State::builder()
            .with_database(StateProviderDatabase::new(&provider))
            .with_bundle_update()
            .build();
        db.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("L1 block info must load");
        let mut builder = ctx.block_builder(&mut db).expect("block builder");
        builder.apply_pre_execution_changes().expect("pre-execution changes");
        let mut info = ctx.execute_sequencer_transactions(&mut builder).expect("sequencer");
        let block_gas_limit = builder.evm_mut().block().gas_limit();
        info.cumulative_gas_used = block_gas_limit;

        // `CancelOnDrop` flips the shared flag to `CANCELLED` when a clone is dropped.
        drop(ctx.cancel.clone());

        let next_calls = Arc::new(Mutex::new(0));
        let transactions = TestParkableTransactions::counting(
            vec![pool_transaction(0)],
            Arc::default(),
            Arc::clone(&next_calls),
        );

        let cancelled = ctx
            .execute_best_transactions(&mut info, &mut builder, transactions)
            .expect("mempool scan");

        assert!(cancelled.is_some(), "a cancelled job must not be finalized");
        assert_eq!(*next_calls.lock().unwrap(), 0, "the guard must stop before the pool is read");
    }

    #[test]
    fn native_builder_includes_transaction_with_matching_block_predicate() {
        let event_capture = TransactionEventCapture::install();
        let transaction =
            pool_transaction(0).with_validity_predicates(vec![ValidityPredicate::BlockNumber {
                op: ValidityOperator::Equal,
                value: U256::ONE,
            }]);
        let sender = transaction.sender();
        let transaction_hash = *transaction.hash();

        let BuildOutcomeKind::Freeze(payload) = build_parkable_pool_payload(
            pool_payload_context(DENIM_TIMESTAMP),
            TestParkableTransactions::new(vec![transaction]),
            &[sender],
        ) else {
            panic!("Denim payload must freeze")
        };

        assert_eq!(payload.block().body().transactions.len(), 1);
        assert_eq!(*payload.block().body().transactions[0].tx_hash(), transaction_hash);

        let transaction_events = event_capture
            .events()
            .into_iter()
            .filter(|event| event.tx_hash == Some(transaction_hash))
            .collect::<Vec<_>>();
        let event_types =
            transaction_events.iter().map(|event| event.event_type).collect::<Vec<_>>();
        assert_eq!(event_types, [TransactionEventType::BuilderAccepted]);
        assert_eq!(transaction_events[0].data["builder_mode"], "native");
    }

    #[test]
    fn native_builder_rejects_validity_candidate_that_fails_after_its_predicates_match() {
        let event_capture = TransactionEventCapture::install();
        // The funded sender is at nonce 0, so nonce 5 fails EVM validation after the predicate
        // has matched.
        let transaction =
            pool_transaction(5).with_validity_predicates(vec![ValidityPredicate::BlockNumber {
                op: ValidityOperator::Equal,
                value: U256::ONE,
            }]);
        let sender = transaction.sender();
        let transaction_hash = *transaction.hash();

        let BuildOutcomeKind::Freeze(payload) = build_parkable_pool_payload(
            pool_payload_context(DENIM_TIMESTAMP),
            TestParkableTransactions::new(vec![transaction]),
            &[sender],
        ) else {
            panic!("Denim payload must freeze")
        };

        assert!(payload.block().body().transactions.is_empty());
        let rejections = event_capture
            .events()
            .into_iter()
            .filter(|event| event.tx_hash == Some(transaction_hash))
            .map(|event| (event.event_type, event.data["rejection_reason"].clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            rejections,
            [(TransactionEventType::BuilderRejected, "invalid_transaction".into())]
        );
    }

    /// A promoted transaction whose predicate is broken again before it is re-yielded is parked a
    /// second time for the same reason, which the journal records once.
    #[test]
    fn native_builder_reports_a_repeated_deferral_once() {
        let event_capture = TransactionEventCapture::install();
        // Event capture is process-global, so these transactions must not share hashes with other
        // tests.
        let watched_address = Address::repeat_byte(0x46);
        let gated = pool_transaction_to(0, Address::repeat_byte(0x57), U256::ZERO)
            .with_validity_predicates(vec![ValidityPredicate::Balance {
                address: watched_address,
                op: ValidityOperator::Equal,
                value: U256::ONE,
            }]);
        // The first trigger satisfies the predicate and promotes `gated`; the second breaks it
        // again before `gated` is re-yielded.
        let satisfy = pool_transaction_to(0, watched_address, U256::ONE);
        let unsatisfy = pool_transaction_to(0, watched_address, U256::from(2));
        let funded_senders = [gated.sender(), satisfy.sender(), unsatisfy.sender()];
        let gated_hash = *gated.hash();

        let BuildOutcomeKind::Freeze(payload) = build_parkable_pool_payload(
            pool_payload_context(DENIM_TIMESTAMP),
            TestParkableTransactions::with_promoted_last(vec![gated, satisfy, unsatisfy]),
            &funded_senders,
        ) else {
            panic!("Denim payload must freeze")
        };

        assert!(
            !payload.block().body().transactions.iter().any(|tx| *tx.tx_hash() == gated_hash),
            "gated transaction must stay parked"
        );
        let gated_events = event_capture
            .events()
            .into_iter()
            .filter(|event| event.tx_hash == Some(gated_hash))
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert_eq!(gated_events, [TransactionEventType::BuilderDeferred]);
    }

    #[test]
    fn native_builder_rejects_flashblock_index_predicates() {
        let transaction = pool_transaction(0).with_validity_predicates(vec![
            ValidityPredicate::FlashblockIndex { op: ValidityOperator::Equal, value: U256::ZERO },
        ]);
        let sender = transaction.sender();

        let BuildOutcomeKind::Freeze(payload) = build_parkable_pool_payload(
            pool_payload_context(DENIM_TIMESTAMP),
            TestParkableTransactions::new(vec![transaction]),
            &[sender],
        ) else {
            panic!("Denim payload must freeze")
        };

        assert!(payload.block().body().transactions.is_empty());
    }

    #[test]
    fn native_builder_skips_expired_block_predicates() {
        let transaction =
            pool_transaction(0).with_validity_predicates(vec![ValidityPredicate::BlockNumber {
                op: ValidityOperator::Equal,
                value: U256::ZERO,
            }]);
        let sender = transaction.sender();

        let BuildOutcomeKind::Freeze(payload) = build_parkable_pool_payload(
            pool_payload_context(DENIM_TIMESTAMP),
            TestParkableTransactions::new(vec![transaction]),
            &[sender],
        ) else {
            panic!("Denim payload must freeze")
        };

        assert!(payload.block().body().transactions.is_empty());
    }

    #[test]
    fn native_builder_promotes_transaction_after_watched_nonce_advances() {
        let trigger = pool_transaction_to(0, Address::repeat_byte(0x64), U256::ZERO);
        let gated = pool_transaction_to(0, Address::repeat_byte(0x65), U256::ZERO)
            .with_validity_predicates(vec![ValidityPredicate::Nonce {
                address: trigger.sender(),
                op: ValidityOperator::Equal,
                value: U256::ONE,
            }]);
        let funded_senders = [gated.sender(), trigger.sender()];
        let gated_hash = *gated.hash();
        let trigger_hash = *trigger.hash();

        let BuildOutcomeKind::Freeze(payload) = build_parkable_pool_payload(
            pool_payload_context(DENIM_TIMESTAMP),
            TestParkableTransactions::new(vec![gated, trigger]),
            &funded_senders,
        ) else {
            panic!("Denim payload must freeze")
        };

        let included_hashes = payload
            .block()
            .body()
            .transactions
            .iter()
            .map(|transaction| *transaction.tx_hash())
            .collect::<Vec<_>>();
        assert_eq!(included_hashes, vec![trigger_hash, gated_hash]);
    }

    #[test]
    fn native_builder_promotes_transaction_after_predicate_state_changes() {
        let watched_address = Address::repeat_byte(0x44);
        let gated = pool_transaction_to(0, Address::repeat_byte(0x55), U256::ZERO)
            .with_validity_predicates(vec![ValidityPredicate::Balance {
                address: watched_address,
                op: ValidityOperator::Equal,
                value: U256::ONE,
            }]);
        let trigger = pool_transaction_to(0, watched_address, U256::ONE);
        let funded_senders = [gated.sender(), trigger.sender()];
        let gated_hash = *gated.hash();
        let trigger_hash = *trigger.hash();

        let BuildOutcomeKind::Freeze(payload) = build_parkable_pool_payload(
            pool_payload_context(DENIM_TIMESTAMP),
            TestParkableTransactions::new(vec![gated, trigger]),
            &funded_senders,
        ) else {
            panic!("Denim payload must freeze")
        };

        let included_hashes = payload
            .block()
            .body()
            .transactions
            .iter()
            .map(|transaction| *transaction.tx_hash())
            .collect::<Vec<_>>();
        assert_eq!(included_hashes, vec![trigger_hash, gated_hash]);
    }

    #[test]
    fn native_builder_bounds_initial_and_rescan_predicate_evaluation() {
        let watched_address = Address::repeat_byte(0x44);
        let gated = pool_transaction_to(0, Address::repeat_byte(0x55), U256::ZERO)
            .with_validity_predicates(vec![ValidityPredicate::Balance {
                address: watched_address,
                op: ValidityOperator::Equal,
                value: U256::ONE,
            }]);
        let matching =
            pool_transaction(1).with_validity_predicates(vec![ValidityPredicate::BlockNumber {
                op: ValidityOperator::Equal,
                value: U256::ONE,
            }]);
        let trigger = pool_transaction_to(0, watched_address, U256::ONE);
        let funded_senders = [gated.sender(), matching.sender(), trigger.sender()];
        let trigger_hash = *trigger.hash();
        let mut ctx = pool_payload_context(DENIM_TIMESTAMP);
        ctx.builder_config.predicate_eval_hard_cutoff = Duration::from_nanos(1);

        let BuildOutcomeKind::Freeze(payload) = build_parkable_pool_payload(
            ctx,
            TestParkableTransactions::new(vec![gated, matching, trigger]),
            &funded_senders,
        ) else {
            panic!("Denim payload must freeze")
        };

        let included_hashes = payload
            .block()
            .body()
            .transactions
            .iter()
            .map(|transaction| *transaction.tx_hash())
            .collect::<Vec<_>>();
        assert_eq!(included_hashes, vec![trigger_hash]);
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

    #[test]
    fn cancelled_ctx_stops_witness() {
        let mut ctx = pool_payload_context(DENIM_TIMESTAMP - 1);
        ctx.config.attributes.transactions = vec![sequencer_attribute_tx(&pool_transaction(0))];
        drop(ctx.cancel.clone());

        let err = Builder::new(|_| NoopPayloadTransactions::<BasePooledTransaction>::default())
            .witness(test_state_provider(), NoopProvider::default(), &ctx)
            .unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
    }

    #[derive(Debug)]
    struct MapProvider(Mutex<HashMap<TxHash, MeterBundleResponse>>);

    impl MeteringProvider for MapProvider {
        fn get(&self, tx_hash: &TxHash) -> Option<Arc<MeterBundleResponse>> {
            self.0.lock().unwrap().get(tx_hash).cloned().map(Arc::new)
        }
    }

    fn cpu_schedule(
        block_limit: u64,
        transaction_limit: Option<u64>,
        dry_run: bool,
    ) -> ResourceMeteringSchedule {
        ResourceMeteringSchedule::new(vec![ResourceMeteringDimension {
            name: "cpu".to_string(),
            block_limit,
            transaction_limit: transaction_limit.unwrap_or(block_limit),
            base_gas_weight: 1,
            operations: Vec::new(),
            dry_run,
        }])
    }

    fn metering_config(
        schedule: ResourceMeteringSchedule,
        provider: SharedMeteringProvider,
    ) -> ResourceMeteringConfig {
        ResourceMeteringConfig {
            enabled: true,
            schedule: Arc::new(schedule.compile().unwrap()),
            provider,
        }
    }

    fn meter_for(tx_hash: TxHash, gas_used: u64) -> MeterBundleResponse {
        MeterBundleResponse {
            results: vec![TransactionResult {
                coinbase_diff: Default::default(),
                eth_sent_to_coinbase: Default::default(),
                from_address: Default::default(),
                gas_fees: Default::default(),
                gas_price: Default::default(),
                gas_used,
                to_address: None,
                tx_hash,
                value: Default::default(),
                execution_time_us: 0,
                opcode_gas: Vec::new(),
            }],
            ..Default::default()
        }
    }

    fn overflowing_schedule() -> ResourceMeteringSchedule {
        ResourceMeteringSchedule::new(vec![ResourceMeteringDimension {
            name: "cpu".to_string(),
            block_limit: 1,
            transaction_limit: 1,
            base_gas_weight: u64::MAX,
            operations: vec![ResourceMeteringOperation {
                name: "SSTORE".to_string(),
                gas_used_weight: u64::MAX,
                count_cost: 0,
            }],
            dry_run: false,
        }])
    }

    fn overflowing_meter(tx_hash: TxHash) -> MeterBundleResponse {
        MeterBundleResponse {
            results: vec![TransactionResult {
                coinbase_diff: Default::default(),
                eth_sent_to_coinbase: Default::default(),
                from_address: Default::default(),
                gas_fees: Default::default(),
                gas_price: Default::default(),
                gas_used: u64::MAX,
                to_address: None,
                tx_hash,
                value: Default::default(),
                execution_time_us: 0,
                opcode_gas: vec![OpcodeGas {
                    contract_address: Default::default(),
                    opcode: "SSTORE".to_string(),
                    count: 1,
                    gas_used: u64::MAX,
                }],
            }],
            ..Default::default()
        }
    }

    fn pool_scan_outcome(
        resource_metering: ResourceMeteringConfig,
        tx: BasePooledTransaction,
        evicted: Arc<Mutex<Vec<TxHash>>>,
        invalid: Arc<Mutex<Vec<(Address, u64)>>>,
    ) -> BuildOutcomeKind<crate::BaseBuiltPayload<BasePrimitives>> {
        pool_scan_outcomes(resource_metering, vec![tx], evicted, invalid)
    }

    fn pool_scan_outcomes(
        resource_metering: ResourceMeteringConfig,
        txs: Vec<BasePooledTransaction>,
        evicted: Arc<Mutex<Vec<TxHash>>>,
        invalid: Arc<Mutex<Vec<(Address, u64)>>>,
    ) -> BuildOutcomeKind<crate::BaseBuiltPayload<BasePrimitives>> {
        let mut ctx = pool_payload_context(DENIM_TIMESTAMP - 1);
        ctx.builder_config.resource_metering = resource_metering;
        let transactions = TestParkableTransactions::recording(txs, invalid);
        build_pool_payload_with(ctx, transactions, move |hashes| {
            evicted.lock().unwrap().extend(hashes);
        })
    }

    fn included_tx_count(
        outcome: BuildOutcomeKind<crate::BaseBuiltPayload<BasePrimitives>>,
    ) -> usize {
        match outcome {
            BuildOutcomeKind::Better { payload } | BuildOutcomeKind::Freeze(payload) => {
                payload.block().body().transactions.len()
            }
            other => panic!("expected a built payload, got {other:?}"),
        }
    }

    #[test]
    fn predicted_transaction_scope_exclude_evicts_from_pool() {
        let tx = pool_transaction(0);
        let tx_hash = *tx.hash();
        let provider: SharedMeteringProvider =
            Arc::new(MapProvider(Mutex::new(HashMap::from([(
                tx_hash,
                meter_for(tx_hash, 21_000),
            )]))));
        let evicted = Arc::new(Mutex::new(Vec::new()));
        let invalid = Arc::new(Mutex::new(Vec::new()));
        let outcome = pool_scan_outcome(
            metering_config(cpu_schedule(1_000_000, Some(100), false), provider),
            tx,
            Arc::clone(&evicted),
            Arc::clone(&invalid),
        );

        assert_eq!(included_tx_count(outcome), 0);
        assert_eq!(*evicted.lock().unwrap(), vec![tx_hash]);
        assert_eq!(invalid.lock().unwrap().len(), 1);
    }

    #[test]
    fn executed_transaction_scope_exclude_evicts_from_pool() {
        let tx = pool_transaction(0);
        let tx_hash = *tx.hash();
        let evicted = Arc::new(Mutex::new(Vec::new()));
        let invalid = Arc::new(Mutex::new(Vec::new()));
        let outcome = pool_scan_outcome(
            metering_config(
                cpu_schedule(1_000_000, Some(100), false),
                Arc::new(NoopMeteringProvider),
            ),
            tx,
            Arc::clone(&evicted),
            Arc::clone(&invalid),
        );

        assert_eq!(included_tx_count(outcome), 0);
        assert_eq!(*evicted.lock().unwrap(), vec![tx_hash]);
        assert_eq!(invalid.lock().unwrap().len(), 1);
    }

    #[test]
    fn block_scope_exclude_skips_scan_without_pool_eviction() {
        let first = pool_transaction(0);
        let second = pool_transaction(1);
        let evicted = Arc::new(Mutex::new(Vec::new()));
        let invalid = Arc::new(Mutex::new(Vec::new()));
        // Own cost (~21_000) is ≤ block_limit so an omitted transaction limit still
        // classifies the second tx as block-scope after the first fills the budget.
        let outcome = pool_scan_outcomes(
            metering_config(cpu_schedule(30_000, None, false), Arc::new(NoopMeteringProvider)),
            vec![first, second],
            Arc::clone(&evicted),
            Arc::clone(&invalid),
        );

        assert_eq!(included_tx_count(outcome), 1);
        assert!(evicted.lock().unwrap().is_empty());
        assert_eq!(invalid.lock().unwrap().len(), 1);
    }

    #[test]
    fn predicted_block_scope_exclude_skips_scan_without_pool_eviction() {
        let first = pool_transaction(0);
        let second = pool_transaction(1);
        let first_hash = *first.hash();
        let second_hash = *second.hash();
        let provider: SharedMeteringProvider = Arc::new(MapProvider(Mutex::new(HashMap::from([
            (first_hash, meter_for(first_hash, 21_000)),
            (second_hash, meter_for(second_hash, 21_000)),
        ]))));
        let evicted = Arc::new(Mutex::new(Vec::new()));
        let invalid = Arc::new(Mutex::new(Vec::new()));
        let outcome = pool_scan_outcomes(
            metering_config(cpu_schedule(30_000, None, false), provider),
            vec![first, second],
            Arc::clone(&evicted),
            Arc::clone(&invalid),
        );

        assert_eq!(included_tx_count(outcome), 1);
        assert!(evicted.lock().unwrap().is_empty());
        assert_eq!(invalid.lock().unwrap().len(), 1);
    }

    #[test]
    fn dry_run_transaction_scope_does_not_evict() {
        let tx = pool_transaction(0);
        let tx_hash = *tx.hash();
        let provider: SharedMeteringProvider =
            Arc::new(MapProvider(Mutex::new(HashMap::from([(
                tx_hash,
                meter_for(tx_hash, 21_000),
            )]))));
        let evicted = Arc::new(Mutex::new(Vec::new()));
        let invalid = Arc::new(Mutex::new(Vec::new()));
        let outcome = pool_scan_outcome(
            metering_config(cpu_schedule(1_000_000, Some(100), true), provider),
            tx,
            Arc::clone(&evicted),
            Arc::clone(&invalid),
        );

        assert_eq!(included_tx_count(outcome), 1);
        assert!(evicted.lock().unwrap().is_empty());
        assert!(invalid.lock().unwrap().is_empty());
    }

    #[test]
    fn calculation_failure_does_not_evict() {
        let tx = pool_transaction(0);
        let tx_hash = *tx.hash();
        let provider: SharedMeteringProvider =
            Arc::new(MapProvider(Mutex::new(HashMap::from([(
                tx_hash,
                overflowing_meter(tx_hash),
            )]))));
        let evicted = Arc::new(Mutex::new(Vec::new()));
        let invalid = Arc::new(Mutex::new(Vec::new()));
        let outcome = pool_scan_outcome(
            metering_config(overflowing_schedule(), provider),
            tx,
            Arc::clone(&evicted),
            Arc::clone(&invalid),
        );

        assert_eq!(included_tx_count(outcome), 1);
        assert!(evicted.lock().unwrap().is_empty());
        assert!(invalid.lock().unwrap().is_empty());
    }

    fn pool_scan_with_ctx(
        ctx: BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>,
        tx: BasePooledTransaction,
        evicted: Arc<Mutex<Vec<TxHash>>>,
        invalid: Arc<Mutex<Vec<(Address, u64)>>>,
    ) -> BuildOutcomeKind<crate::BaseBuiltPayload<BasePrimitives>> {
        let transactions = TestParkableTransactions::recording(vec![tx], invalid);
        build_pool_payload_with(ctx, transactions, move |hashes| {
            evicted.lock().unwrap().extend(hashes);
        })
    }

    #[test]
    fn rejected_hash_skipped_on_subsequent_job_even_if_in_pool() {
        let tx = pool_transaction(0);
        let tx_hash = *tx.hash();
        let provider: SharedMeteringProvider =
            Arc::new(MapProvider(Mutex::new(HashMap::from([(
                tx_hash,
                meter_for(tx_hash, 21_000),
            )]))));
        let mut ctx = pool_payload_context(DENIM_TIMESTAMP - 1);
        ctx.builder_config.resource_metering =
            metering_config(cpu_schedule(1_000_000, Some(100), false), provider);
        let cache = ctx.builder_config.rejection_cache.clone();

        let evicted = Arc::new(Mutex::new(Vec::new()));
        let invalid = Arc::new(Mutex::new(Vec::new()));
        let outcome =
            pool_scan_with_ctx(ctx, tx.clone(), Arc::clone(&evicted), Arc::clone(&invalid));

        assert_eq!(included_tx_count(outcome), 0);
        assert_eq!(*evicted.lock().unwrap(), vec![tx_hash]);
        assert!(cache.is_rejected(&tx_hash));

        // Later job: the tx is in the iterator again (P2P re-insert). Metering is
        // fail-open without a sample, so a skip must come from the shared cache.
        let mut ctx = pool_payload_context(DENIM_TIMESTAMP - 1);
        ctx.builder_config.rejection_cache = cache;
        ctx.builder_config.resource_metering = metering_config(
            cpu_schedule(1_000_000, Some(100), false),
            Arc::new(NoopMeteringProvider),
        );
        let evicted = Arc::new(Mutex::new(Vec::new()));
        let invalid = Arc::new(Mutex::new(Vec::new()));
        let outcome = pool_scan_with_ctx(ctx, tx, Arc::clone(&evicted), Arc::clone(&invalid));

        assert_eq!(included_tx_count(outcome), 0);
        assert!(evicted.lock().unwrap().is_empty());
        assert_eq!(invalid.lock().unwrap().len(), 1);
    }

    #[test]
    fn block_scope_exclude_is_not_cached() {
        let first = pool_transaction(0);
        let second = pool_transaction(1);
        let second_hash = *second.hash();
        let mut ctx = pool_payload_context(DENIM_TIMESTAMP - 1);
        ctx.builder_config.resource_metering =
            metering_config(cpu_schedule(30_000, None, false), Arc::new(NoopMeteringProvider));
        let cache = ctx.builder_config.rejection_cache.clone();

        let evicted = Arc::new(Mutex::new(Vec::new()));
        let invalid = Arc::new(Mutex::new(Vec::new()));
        let transactions = TestParkableTransactions::recording(vec![first, second], invalid);
        let outcome = build_pool_payload_with(ctx, transactions, {
            let evicted = Arc::clone(&evicted);
            move |hashes| {
                evicted.lock().unwrap().extend(hashes);
            }
        });

        assert_eq!(included_tx_count(outcome), 1);
        assert!(evicted.lock().unwrap().is_empty());
        assert!(!cache.is_rejected(&second_hash));
    }

    #[test]
    fn dry_run_transaction_scope_is_not_cached() {
        let tx = pool_transaction(0);
        let tx_hash = *tx.hash();
        let provider: SharedMeteringProvider =
            Arc::new(MapProvider(Mutex::new(HashMap::from([(
                tx_hash,
                meter_for(tx_hash, 21_000),
            )]))));
        let mut ctx = pool_payload_context(DENIM_TIMESTAMP - 1);
        ctx.builder_config.resource_metering =
            metering_config(cpu_schedule(1_000_000, Some(100), true), provider);
        let cache = ctx.builder_config.rejection_cache.clone();

        let evicted = Arc::new(Mutex::new(Vec::new()));
        let invalid = Arc::new(Mutex::new(Vec::new()));
        let outcome = pool_scan_with_ctx(ctx, tx, Arc::clone(&evicted), Arc::clone(&invalid));

        assert_eq!(included_tx_count(outcome), 1);
        assert!(evicted.lock().unwrap().is_empty());
        assert!(!cache.is_rejected(&tx_hash));
    }

    #[test]
    fn calculation_failure_is_not_cached() {
        let tx = pool_transaction(0);
        let tx_hash = *tx.hash();
        let provider: SharedMeteringProvider =
            Arc::new(MapProvider(Mutex::new(HashMap::from([(
                tx_hash,
                overflowing_meter(tx_hash),
            )]))));
        let mut ctx = pool_payload_context(DENIM_TIMESTAMP - 1);
        ctx.builder_config.resource_metering = metering_config(overflowing_schedule(), provider);
        let cache = ctx.builder_config.rejection_cache.clone();

        let evicted = Arc::new(Mutex::new(Vec::new()));
        let invalid = Arc::new(Mutex::new(Vec::new()));
        let outcome = pool_scan_with_ctx(ctx, tx, Arc::clone(&evicted), Arc::clone(&invalid));

        assert_eq!(included_tx_count(outcome), 1);
        assert!(evicted.lock().unwrap().is_empty());
        assert!(!cache.is_rejected(&tx_hash));
    }

    fn sequencer_attribute_tx(tx: &BasePooledTransaction) -> WithEncoded<BaseTxEnvelope> {
        let encoded = tx.encoded_2718().clone();
        let envelope = tx.clone_into_consensus().into_inner();
        WithEncoded::new(encoded, envelope)
    }

    fn meter_with_sstore(tx_hash: TxHash, gas_used: u64, sstore_count: u64) -> MeterBundleResponse {
        MeterBundleResponse {
            results: vec![TransactionResult {
                coinbase_diff: Default::default(),
                eth_sent_to_coinbase: Default::default(),
                from_address: Default::default(),
                gas_fees: Default::default(),
                gas_price: Default::default(),
                gas_used,
                to_address: None,
                tx_hash,
                value: Default::default(),
                execution_time_us: 0,
                opcode_gas: vec![OpcodeGas {
                    contract_address: Default::default(),
                    opcode: "SSTORE".to_string(),
                    count: sstore_count,
                    gas_used: 0,
                }],
            }],
            ..Default::default()
        }
    }

    fn cpu_schedule_with_sstore(
        block_limit: u64,
        transaction_limit: Option<u64>,
        sstore_count_cost: u64,
    ) -> ResourceMeteringSchedule {
        ResourceMeteringSchedule::new(vec![ResourceMeteringDimension {
            name: "cpu".to_string(),
            block_limit,
            transaction_limit: transaction_limit.unwrap_or(block_limit),
            base_gas_weight: 1,
            operations: vec![ResourceMeteringOperation {
                name: "SSTORE".to_string(),
                gas_used_weight: 0,
                count_cost: sstore_count_cost,
            }],
            dry_run: false,
        }])
    }

    fn test_state_provider() -> StateProviderTest {
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
        provider
    }

    #[test]
    fn sequencer_over_budget_is_included_and_counted_against_mempool() {
        let sequencer = pool_transaction(0);
        let mempool = pool_transaction(1);
        let sequencer_hash = *sequencer.hash();
        let provider: SharedMeteringProvider =
            Arc::new(MapProvider(Mutex::new(HashMap::from([(
                sequencer_hash,
                meter_with_sstore(sequencer_hash, 21_000, 3),
            )]))));
        let mut ctx = pool_payload_context(DENIM_TIMESTAMP - 1);
        ctx.config.attributes.transactions = vec![sequencer_attribute_tx(&sequencer)];
        // Sequencer cost is 21_000 + 3 * 10_000 = 51_000, which exceeds block_limit.
        // Mempool cost is ~21_000, which would fit an empty block.
        ctx.builder_config.resource_metering =
            metering_config(cpu_schedule_with_sstore(40_000, None, 10_000), provider);

        let evicted = Arc::new(Mutex::new(Vec::new()));
        let invalid = Arc::new(Mutex::new(Vec::new()));
        let transactions = TestParkableTransactions::recording(vec![mempool], Arc::clone(&invalid));
        let outcome = build_pool_payload_with(ctx, transactions, {
            let evicted = Arc::clone(&evicted);
            move |hashes| {
                evicted.lock().unwrap().extend(hashes);
            }
        });

        let payload = match outcome {
            BuildOutcomeKind::Better { payload } | BuildOutcomeKind::Freeze(payload) => payload,
            other => panic!("expected a built payload, got {other:?}"),
        };
        let hashes: Vec<_> =
            payload.block().body().transactions.iter().map(|tx| *tx.tx_hash()).collect();
        assert_eq!(hashes, vec![sequencer_hash]);
        assert!(evicted.lock().unwrap().is_empty());
        assert_eq!(invalid.lock().unwrap().len(), 1);
    }

    #[test]
    fn executed_throttle_discards_state_and_usage() {
        let tx = pool_transaction(0);
        let sender = tx.sender();
        let mut ctx = pool_payload_context(DENIM_TIMESTAMP - 1);
        ctx.builder_config.resource_metering = metering_config(
            cpu_schedule(1_000_000, Some(100), false),
            Arc::new(NoopMeteringProvider),
        );
        let provider = test_state_provider();
        let mut db = State::builder()
            .with_database(StateProviderDatabase::new(&provider))
            .with_bundle_update()
            .build();
        db.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("L1 block info must load");
        let mut builder = ctx.block_builder(&mut db).expect("block builder");
        builder.apply_pre_execution_changes().expect("pre-execution changes");
        let mut info = ctx.execute_sequencer_transactions(&mut builder).expect("sequencer");
        let usage_before = info.resource_metering_usage.clone();
        let nonce_before = builder
            .evm_mut()
            .db_mut()
            .basic(sender)
            .ok()
            .flatten()
            .map(|account| account.nonce)
            .unwrap_or(0);

        let invalid = Arc::new(Mutex::new(Vec::new()));
        let transactions = TestParkableTransactions::recording(vec![tx], invalid);
        ctx.execute_best_transactions(&mut info, &mut builder, transactions).expect("mempool scan");
        let nonce_after = builder
            .evm_mut()
            .db_mut()
            .basic(sender)
            .ok()
            .flatten()
            .map(|account| account.nonce)
            .unwrap_or(0);

        assert_eq!(info.resource_metering_usage, usage_before);
        assert_eq!(nonce_after, nonce_before);
        assert_eq!(info.cumulative_gas_used, 0);
        assert!(!info.permanently_rejected_txs.is_empty());
    }

    #[test]
    fn check_simulated_usage_missing_meter_data_still_executes() {
        let tx = pool_transaction(0);
        let evicted = Arc::new(Mutex::new(Vec::new()));
        let invalid = Arc::new(Mutex::new(Vec::new()));
        let outcome = pool_scan_outcome(
            metering_config(
                cpu_schedule(1_000_000, Some(1_000_000), false),
                Arc::new(NoopMeteringProvider),
            ),
            tx,
            Arc::clone(&evicted),
            Arc::clone(&invalid),
        );

        assert_eq!(included_tx_count(outcome), 1);
        assert!(evicted.lock().unwrap().is_empty());
        assert!(invalid.lock().unwrap().is_empty());
    }

    /// The build loop wraps its state provider in `InstrumentedStateProvider` *outside*
    /// `CachedStateProvider`, so the recorded latency is the total cost of a read as the builder
    /// experiences it. This asserts the consequence: a cache hit and a read-through are both
    /// timed. Wrapping the other way round would record only the read-through, and a warm cache
    /// would then look like no improvement rather than a faster one.
    ///
    /// Hits are told apart from read-throughs by value: the cache and the provider underneath
    /// hold different balances for the same address, so the returned balance names the source.
    /// `CachedStateProvider::new` is lookup-only and never populates on a miss, so the cached
    /// entry here stands in for one the engine placed there.
    #[test]
    fn instrumentation_times_both_cache_hits_and_read_throughs() {
        const FROM_CACHE: u64 = 1;
        const FROM_PROVIDER: u64 = 999;

        let cached_address = Address::with_last_byte(0x11);
        let uncached_address = Address::with_last_byte(0x22);

        let mut provider = reth_revm::test_utils::StateProviderTest::default();
        for address in [cached_address, uncached_address] {
            provider.insert_account(
                address,
                reth_primitives_traits::Account {
                    balance: U256::from(FROM_PROVIDER),
                    ..Default::default()
                },
                None,
                HashMap::default(),
            );
        }

        let cache = reth_execution_cache::ExecutionCache::new(1_000_000);
        cache.insert_account(
            cached_address,
            Some(reth_primitives_traits::Account {
                balance: U256::from(FROM_CACHE),
                ..Default::default()
            }),
        );

        let instrumented = crate::BuilderStateProvider::new(provider, Some(cache), true);
        let stats = Arc::clone(instrumented.stats().expect("instrumentation enabled"));

        let hit = reth_storage_api::AccountReader::basic_account(
            instrumented.provider(),
            &cached_address,
        )
        .unwrap()
        .expect("cached account");
        assert_eq!(
            hit.balance,
            U256::from(FROM_CACHE),
            "read should have been served by the cache, not the provider underneath"
        );

        let miss = reth_storage_api::AccountReader::basic_account(
            instrumented.provider(),
            &uncached_address,
        )
        .unwrap()
        .expect("uncached account");
        assert_eq!(
            miss.balance,
            U256::from(FROM_PROVIDER),
            "read should have fallen through to the provider underneath"
        );

        assert_eq!(
            stats.total_account_fetches(),
            2,
            "the cache hit must be timed as well as the read-through; recording only the \
             read-through would hide the benefit of a warm cache"
        );
    }
}
