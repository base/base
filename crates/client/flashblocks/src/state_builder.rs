use std::sync::Arc;

use alloy_consensus::{
    Block, Header, TxReceipt,
    transaction::{Recovered, TransactionMeta},
};
use alloy_primitives::B256;
use alloy_rpc_types::TransactionTrait;
use alloy_rpc_types_eth::state::StateOverride;
use op_alloy_consensus::{OpReceipt, OpTxEnvelope};
use op_alloy_rpc_types::{OpTransactionReceipt, Transaction};
use reth_evm::{
    Evm, FromRecoveredTx,
    op_revm::{L1BlockInfo, OpHaltReason},
};
use reth_optimism_chainspec::OpHardforks;
use reth_optimism_primitives::OpPrimitives;
use reth_optimism_rpc::OpReceiptBuilder as OpRpcReceiptBuilder;
use reth_rpc_convert::transaction::ConvertReceiptInput;
use revm::{
    Database, DatabaseCommit,
    context::result::{ExecutionResult, ResultAndState},
    state::EvmState,
};

use crate::{ExecutionError, PendingBlocks, StateProcessorError, UnifiedReceiptBuilder};

/// Represents the result of executing or fetching a cached pending transaction.
#[derive(Debug, Clone)]
pub struct ExecutedPendingTransaction {
    /// The RPC transaction.
    pub rpc_transaction: Transaction,
    /// The receipt of the transaction.
    pub receipt: OpTransactionReceipt,
    /// The updated EVM state.
    pub state: EvmState,
    /// The result of the transaction.
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
            let result = p.get_transaction_result(tx_hash)?;
            Some((receipt, state, result))
        });

        // If cached, we can fill out pending block data using previous execution results
        // If not cached, we need to execute the transaction and build pending block data from scratch
        if let Some((receipt, state, result)) = cached_data {
            self.execute_with_cached_data(
                transaction,
                receipt,
                result,
                state,
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
        result: ExecutionResult<OpHaltReason>,
        state: EvmState,
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

                    let existing = existing_override.state_diff.get_or_insert(Default::default());
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
                    result.clone(),
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
                reason: format!("{:?}", e),
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
