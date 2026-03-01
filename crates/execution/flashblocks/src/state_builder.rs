use std::sync::{Arc, atomic::AtomicBool, mpsc::Sender};

use alloy_consensus::{
    Block, Header, TxReceipt,
    transaction::{Recovered, TransactionMeta},
};
use alloy_eips::eip7928::BlockAccessList;
use alloy_primitives::{Address, B256};
use alloy_rpc_types::TransactionTrait;
use alloy_rpc_types_eth::state::StateOverride;
use base_alloy_consensus::{OpReceipt, OpTxEnvelope};
use base_alloy_rpc_types::{OpTransactionReceipt, Transaction};
use base_execution_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use base_execution_forks::OpHardforks;
use base_execution_primitives::OpPrimitives;
use base_execution_rpc::OpReceiptBuilder as OpRpcReceiptBuilder;
use base_revm::{L1BlockInfo, OpHaltReason};
use reth_engine_tree::tree::{
    CachedStateMetrics, ExecutionCache, ExecutionEnv, PayloadExecutionCache, SavedCache,
    StateProviderBuilder,
    precompile_cache::PrecompileCacheMap,
    prewarm::{PrewarmCacheTask, PrewarmContext, PrewarmMetrics, PrewarmMode, PrewarmTaskEvent},
};
use reth_evm::{ConfigureEvm, Evm, FromRecoveredTx};
use reth_provider::{BlockReader, StateProviderFactory, StateReader};
use reth_rpc_convert::transaction::ConvertReceiptInput;
use reth_tasks::Runtime;
use revm::{
    Database, DatabaseCommit,
    context::result::{ExecutionResult, ResultAndState},
    state::EvmState,
};
use tracing::debug;

use crate::{ExecutionError, PendingBlocks, StateProcessorError, UnifiedReceiptBuilder};

/// Default cache size in bytes (100MB).
const DEFAULT_CACHE_SIZE: usize = 100_000_000;

/// Handle for managing a running prewarm task.
///
/// Call [`PrewarmHandle::terminate`] when block execution is complete to cleanly
/// shut down the prewarm task. If dropped without calling terminate, the task
/// will exit when its channel is closed.
#[derive(Debug)]
pub struct PrewarmHandle {
    sender: Sender<PrewarmTaskEvent<OpReceipt>>,
}

impl PrewarmHandle {
    /// Signals the prewarm task to terminate.
    ///
    /// This should be called when block execution is complete.
    pub fn terminate(self) {
        // Create a dummy receiver to satisfy the type.
        // With execution_outcome: None, the cache saving logic is skipped.
        let (_tx, rx) = std::sync::mpsc::channel();

        let _ = self
            .sender
            .send(PrewarmTaskEvent::Terminate { execution_outcome: None, valid_block_rx: rx });
    }
}

/// Result of setting up a cache with optional prewarming.
#[derive(Debug)]
pub struct CacheSetupResult {
    /// The execution cache to use with `CachedStateProvider`.
    pub cache: ExecutionCache,
    /// Metrics for the cache.
    pub metrics: CachedStateMetrics,
    /// Optional handle to terminate the prewarm task (only present if BAL was provided).
    pub prewarm_handle: Option<PrewarmHandle>,
}

/// Manages cache prewarming for flashblocks execution.
///
/// This struct encapsulates the setup of `PrewarmCacheTask` following the
/// pattern from reth's `payload_processor`.
#[derive(Debug, Clone)]
pub struct CachePrewarmer<Client> {
    executor: Runtime,
    client: Arc<Client>,
}

impl<Client> CachePrewarmer<Client>
where
    Client: StateProviderFactory + BlockReader + StateReader + Clone + Send + Sync + 'static,
{
    /// Creates a new cache prewarmer.
    pub const fn new(executor: Runtime, client: Arc<Client>) -> Self {
        Self { executor, client }
    }

    /// Sets up a cache and optionally spawns prewarming workers if a BAL is provided.
    ///
    /// Returns `CacheSetupResult` containing the cache components and an optional
    /// handle to terminate the prewarm task.
    ///
    /// If a BAL is provided, background workers will populate the cache in parallel
    /// with execution. Call `prewarm_handle.terminate()` when execution is complete.
    pub fn setup_cache<ChainSpec>(
        &self,
        parent_header: &Header,
        evm_config: &OpEvmConfig<ChainSpec>,
        access_list: Option<BlockAccessList>,
        transaction_count: usize,
    ) -> Result<CacheSetupResult, ExecutionError>
    where
        ChainSpec: reth_chainspec::EthChainSpec<Header = Header>
            + OpHardforks
            + Clone
            + Send
            + Sync
            + 'static,
    {
        let parent_hash = parent_header.hash_slow();

        // Create saved cache for state prewarming
        let saved_cache = SavedCache::new(
            parent_hash,
            ExecutionCache::new(DEFAULT_CACHE_SIZE),
            CachedStateMetrics::zeroed(),
        );

        // If we have a BAL, spawn PrewarmCacheTask in background
        let prewarm_handle = if let Some(bal) = access_list {
            Some(self.spawn_prewarm_task(
                parent_header,
                parent_hash,
                evm_config,
                saved_cache.clone(),
                bal,
                transaction_count,
            )?)
        } else {
            None
        };

        // Split the saved cache to return components for CachedStateProvider
        let (cache, metrics, _disable_metrics) = saved_cache.split();
        Ok(CacheSetupResult { cache, metrics, prewarm_handle })
    }

    /// Spawns the `PrewarmCacheTask` in background and returns a handle to terminate it.
    fn spawn_prewarm_task<ChainSpec>(
        &self,
        parent_header: &Header,
        parent_hash: B256,
        evm_config: &OpEvmConfig<ChainSpec>,
        saved_cache: SavedCache,
        bal: BlockAccessList,
        transaction_count: usize,
    ) -> Result<PrewarmHandle, ExecutionError>
    where
        ChainSpec: reth_chainspec::EthChainSpec<Header = Header>
            + OpHardforks
            + Clone
            + Send
            + Sync
            + 'static,
    {
        debug!(
            message = "spawning PrewarmCacheTask for BAL prewarming",
            parent_hash = %parent_hash
        );

        // Create StateProviderBuilder for prewarm workers
        let provider_builder = StateProviderBuilder::new(
            (*self.client).clone(),
            parent_hash,
            None, // No overlay blocks needed for flashblocks
        );

        // Create a minimal ExecutionEnv (BAL prewarming doesn't use EVM fields)
        let execution_env = ExecutionEnv {
            parent_state_root: parent_header.state_root,
            evm_env: evm_config
                .next_evm_env(
                    parent_header,
                    &OpNextBlockEnvAttributes {
                        timestamp: 0,
                        suggested_fee_recipient: Address::ZERO,
                        prev_randao: B256::ZERO,
                        gas_limit: 0,
                        parent_beacon_block_root: None,
                        extra_data: Default::default(),
                    },
                )
                .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?,
            hash: B256::ZERO,
            parent_hash,
            transaction_count,
            withdrawals: None,
        };

        // Create PrewarmContext
        let prewarm_ctx = PrewarmContext {
            env: execution_env,
            evm_config: evm_config.clone(),
            saved_cache: Some(saved_cache),
            provider: provider_builder,
            metrics: PrewarmMetrics::default(),
            terminate_execution: Arc::new(AtomicBool::new(false)),
            precompile_cache_disabled: true,
            precompile_cache_map: PrecompileCacheMap::default(),
            v2_proofs_enabled: false,
        };

        // Create PrewarmCacheTask
        let (prewarm_task, actions_tx) = PrewarmCacheTask::new(
            self.executor.clone(),
            PayloadExecutionCache::default(),
            prewarm_ctx,
            None,
            0,
        );

        // Clone sender for the handle before moving into spawn
        let handle_sender = actions_tx.clone();

        // Spawn the prewarm task in background (non-blocking)
        let bal = Arc::new(bal);
        self.executor.spawn_blocking(move || {
            prewarm_task
                .run(PrewarmMode::<Recovered<OpTxEnvelope>>::BlockAccessList(bal), actions_tx);
        });

        Ok(PrewarmHandle { sender: handle_sender })
    }
}

/// Represents the result of executing or fetching a cached pending transaction.
#[derive(Debug, Clone)]
pub struct ExecutedPendingTransaction {
    /// The RPC transaction.
    pub rpc_transaction: Transaction,
    /// The receipt of the transaction.
    pub receipt: OpTransactionReceipt,
    /// The updated EVM state.
    pub state: EvmState,
    /// The execution result of the transaction.
    pub result: ExecutionResult<OpHaltReason>,
}

/// Executes or fetches cached values for transactions in a flashblock.
#[derive(Debug)]
pub struct PendingStateBuilder<E, ChainSpec> {
    cumulative_gas_used: u64,
    next_log_index: usize,

    evm: E,
    pending_block: Block<OpTxEnvelope, Header>,
    l1_block_info: L1BlockInfo,
    receipt_builder: UnifiedReceiptBuilder<ChainSpec>,

    prev_pending_blocks: Option<Arc<PendingBlocks>>,
    state_overrides: StateOverride,
}

impl<E, ChainSpec, DB> PendingStateBuilder<E, ChainSpec>
where
    E: Evm<DB = DB, HaltReason = OpHaltReason>,
    DB: Database + DatabaseCommit,
    E::Tx: FromRecoveredTx<OpTxEnvelope>,
    ChainSpec: OpHardforks,
{
    /// Creates a new pending state builder.
    pub const fn new(
        chain_spec: ChainSpec,
        evm: E,
        pending_block: Block<OpTxEnvelope, Header>,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        l1_block_info: L1BlockInfo,
        state_overrides: StateOverride,
    ) -> Self {
        Self {
            pending_block,
            evm,
            cumulative_gas_used: 0,
            next_log_index: 0,
            prev_pending_blocks,
            l1_block_info,
            state_overrides,
            receipt_builder: UnifiedReceiptBuilder::new(chain_spec),
        }
    }

    /// Consumes the builder and returns the database and state overrides.
    pub fn into_db_and_state_overrides(self) -> (DB, StateOverride) {
        (self.evm.into_db(), self.state_overrides)
    }

    /// Executes a batch of transactions and returns the results.
    ///
    /// Returns a `BatchExecutionResult` containing all executed transactions and
    /// access to the final database state. Results can be iterated as an async stream
    /// via `into_stream()`.
    ///
    /// This interface enables future parallel execution with state prewarming while
    /// maintaining sequential result delivery.
    pub fn execute_batch(
        mut self,
        transactions: Vec<Recovered<OpTxEnvelope>>,
    ) -> BatchExecutionResult<DB> {
        // Process all transactions sequentially, collecting results.
        // Future optimization: parallel execution with state prewarming.
        let results: Vec<Result<ExecutedPendingTransaction, StateProcessorError>> = transactions
            .into_iter()
            .enumerate()
            .map(|(idx, transaction)| self.execute_transaction(idx, transaction))
            .collect();

        // Extract DB and state_overrides immediately to avoid holding non-Send EVM across await
        let (db, state_overrides) = self.into_db_and_state_overrides();

        BatchExecutionResult { db, state_overrides, results }
    }

    /// Executes a single transaction and updates internal state.
    /// Should be called in order for each transaction.
    fn execute_transaction(
        &mut self,
        idx: usize,
        transaction: Recovered<OpTxEnvelope>,
    ) -> Result<ExecutedPendingTransaction, StateProcessorError> {
        let tx_hash = transaction.tx_hash();

        let effective_gas_price = if transaction.is_deposit() {
            0
        } else {
            self.pending_block
                .base_fee_per_gas
                .map(|base_fee| {
                    transaction.effective_tip_per_gas(base_fee).unwrap_or_default()
                        + base_fee as u128
                })
                .unwrap_or_else(|| transaction.max_fee_per_gas())
        };

        // Check if we have all the data we need (receipt + state)
        let cached_data = self.prev_pending_blocks.as_ref().and_then(|p| {
            let receipt = p.get_receipt(tx_hash)?;
            let state = p.get_transaction_state(&tx_hash)?;
            let result = p.get_transaction_result(&tx_hash)?;
            Some((receipt, state, result))
        });

        // If cached, we can fill out pending block data using previous execution results
        // If not cached, we need to execute the transaction and build pending block data from scratch
        if let Some((receipt, state, result)) = cached_data {
            self.execute_with_cached_data(
                transaction,
                receipt.clone(),
                state,
                result.clone(),
                idx,
                effective_gas_price,
            )
        } else {
            self.execute_with_evm(transaction, idx, effective_gas_price)
        }
    }

    /// Builds transaction result from cached receipt and state data.
    fn execute_with_cached_data(
        &mut self,
        transaction: Recovered<OpTxEnvelope>,
        receipt: OpTransactionReceipt,
        state: EvmState,
        result: ExecutionResult<OpHaltReason>,
        idx: usize,
        effective_gas_price: u128,
    ) -> Result<ExecutedPendingTransaction, StateProcessorError> {
        let (deposit_receipt_version, deposit_nonce) = if transaction.is_deposit() {
            let OpReceipt::Deposit(deposit_receipt) = &receipt.inner.inner.receipt else {
                return Err(ExecutionError::DepositReceiptMismatch.into());
            };

            (deposit_receipt.deposit_receipt_version, deposit_receipt.deposit_nonce)
        } else {
            (None, None)
        };

        let rpc_transaction = Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: transaction,
                block_hash: None,
                block_number: Some(self.pending_block.number),
                transaction_index: Some(idx as u64),
                effective_gas_price: Some(effective_gas_price),
            },
            deposit_nonce,
            deposit_receipt_version,
        };

        self.cumulative_gas_used = self
            .cumulative_gas_used
            .checked_add(receipt.inner.gas_used)
            .ok_or(ExecutionError::GasOverflow)?;
        self.next_log_index += receipt.inner.logs().len();

        Ok(ExecutedPendingTransaction { rpc_transaction, receipt, state, result })
    }

    /// Executes the transaction through the EVM and builds the result from scratch.
    fn execute_with_evm(
        &mut self,
        transaction: Recovered<OpTxEnvelope>,
        idx: usize,
        effective_gas_price: u128,
    ) -> Result<ExecutedPendingTransaction, StateProcessorError> {
        let tx_hash = transaction.tx_hash();

        match self.evm.transact(&transaction) {
            Ok(ResultAndState { state, result }) => {
                let gas_used = result.gas_used();
                for (addr, acc) in &state {
                    let existing_override = self.state_overrides.entry(*addr).or_default();
                    existing_override.balance = Some(acc.info.balance);
                    existing_override.nonce = Some(acc.info.nonce);
                    existing_override.code = acc.info.code.clone().map(|code| code.bytes());

                    let existing =
                        existing_override.state_diff.get_or_insert_with(Default::default);
                    let changed_slots = acc
                        .storage
                        .iter()
                        .map(|(&key, slot)| (B256::from(key), B256::from(slot.present_value)));

                    existing.extend(changed_slots);
                }

                self.cumulative_gas_used = self
                    .cumulative_gas_used
                    .checked_add(gas_used)
                    .ok_or(ExecutionError::GasOverflow)?;

                // Build receipt using the unified receipt builder - handles both
                // deposit and non-deposit transactions seamlessly
                let receipt = self.receipt_builder.build(
                    &mut self.evm,
                    &transaction,
                    &result,
                    self.cumulative_gas_used,
                    self.pending_block.timestamp,
                )?;

                let meta = TransactionMeta {
                    tx_hash,
                    index: idx as u64,
                    block_hash: B256::ZERO, // block hash is not available yet for flashblocks
                    block_number: self.pending_block.number,
                    base_fee: self.pending_block.base_fee_per_gas,
                    excess_blob_gas: self.pending_block.excess_blob_gas,
                    timestamp: self.pending_block.timestamp,
                };

                let sender = transaction.signer();
                let input: ConvertReceiptInput<'_, OpPrimitives> = ConvertReceiptInput {
                    receipt: receipt.clone(),
                    tx: Recovered::new_unchecked(&transaction, sender),
                    gas_used,
                    next_log_index: self.next_log_index,
                    meta,
                };

                let op_receipt = OpRpcReceiptBuilder::new(
                    self.receipt_builder.chain_spec(),
                    input,
                    &mut self.l1_block_info,
                )
                .map_err(|e| ExecutionError::RpcReceiptBuild(e.to_string()))?
                .build();
                self.next_log_index += receipt.logs().len();

                let (deposit_receipt_version, deposit_nonce) = if transaction.is_deposit() {
                    let OpReceipt::Deposit(deposit_receipt) = &op_receipt.inner.inner.receipt
                    else {
                        return Err(ExecutionError::DepositReceiptMismatch.into());
                    };

                    (deposit_receipt.deposit_receipt_version, deposit_receipt.deposit_nonce)
                } else {
                    (None, None)
                };

                let rpc_transaction = Transaction {
                    inner: alloy_rpc_types_eth::Transaction {
                        inner: transaction,
                        block_hash: None,
                        block_number: Some(self.pending_block.number),
                        transaction_index: Some(idx as u64),
                        effective_gas_price: Some(effective_gas_price),
                    },
                    deposit_nonce,
                    deposit_receipt_version,
                };
                self.evm.db_mut().commit(state.clone());

                Ok(ExecutedPendingTransaction {
                    rpc_transaction,
                    receipt: op_receipt,
                    state,
                    result,
                })
            }
            Err(e) => Err(ExecutionError::TransactionFailed {
                tx_hash,
                sender: transaction.signer(),
                reason: format!("{e:?}"),
            }
            .into()),
        }
    }
}

/// Result of batch execution containing all transaction results and the final state.
///
/// Provides an async stream interface for iterating over results while maintaining
/// access to the final database state after processing.
#[derive(Debug)]
pub struct BatchExecutionResult<DB> {
    db: DB,
    state_overrides: StateOverride,
    results: Vec<Result<ExecutedPendingTransaction, StateProcessorError>>,
}

impl<DB> BatchExecutionResult<DB>
where
    DB: Database + DatabaseCommit,
{
    /// Returns an async stream over the execution results.
    ///
    /// The stream yields results in transaction order. This enables async iteration
    /// patterns and prepares for future parallel execution support.
    pub fn into_stream(
        self,
    ) -> (
        impl futures_util::Stream<Item = Result<ExecutedPendingTransaction, StateProcessorError>>,
        BatchExecutionHandle<DB>,
    ) {
        let handle = BatchExecutionHandle { db: self.db, state_overrides: self.state_overrides };
        let stream = futures_util::stream::iter(self.results);
        (stream, handle)
    }

    /// Consumes the result and returns the database and state overrides.
    ///
    /// Use this when you don't need to iterate over results as a stream.
    pub fn into_db_and_state_overrides(self) -> (DB, StateOverride) {
        (self.db, self.state_overrides)
    }
}

/// Handle for accessing the final database state after batch execution.
///
/// This handle allows retrieval of the database and state overrides after all
/// transactions have been processed through the stream.
#[derive(Debug)]
pub struct BatchExecutionHandle<DB> {
    db: DB,
    state_overrides: StateOverride,
}

impl<DB> BatchExecutionHandle<DB>
where
    DB: Database + DatabaseCommit,
{
    /// Consumes the handle and returns the database and state overrides.
    ///
    /// Should be called after the stream has been fully consumed.
    pub fn into_db_and_state_overrides(self) -> (DB, StateOverride) {
        (self.db, self.state_overrides)
    }
}
