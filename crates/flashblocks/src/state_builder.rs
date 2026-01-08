use std::sync::Arc;

use alloy_consensus::{
    Block, Eip658Value, Header, TxReceipt,
    transaction::{Recovered, TransactionMeta},
};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use alloy_primitives::B256;
use alloy_rpc_types::TransactionTrait;
use alloy_rpc_types_eth::state::StateOverride;
use crate::error::{Result, StateProcessorError};
use op_alloy_consensus::{OpDepositReceipt, OpTxEnvelope};
use op_alloy_rpc_types::{OpTransactionReceipt, Transaction};
use reth::revm::{Database, DatabaseCommit, context::result::ResultAndState, state::EvmState};
use reth_evm::{
    Evm, FromRecoveredTx, eth::receipt_builder::ReceiptBuilderCtx, op_revm::L1BlockInfo,
};
use reth_optimism_chainspec::OpHardforks;
use reth_optimism_evm::OpRethReceiptBuilder;
use reth_optimism_primitives::OpPrimitives;
use reth_optimism_rpc::OpReceiptBuilder as OpRpcReceiptBuilder;
use reth_rpc_convert::transaction::ConvertReceiptInput;

use crate::PendingBlocks;

/// Represents the result of executing or fetching a cached pending transaction.
#[derive(Debug, Clone)]
pub struct ExecutedPendingTransaction {
    /// The RPC transaction.
    pub rpc_transaction: Transaction,
    /// The receipt of the transaction.
    pub receipt: OpTransactionReceipt,
    /// The updated EVM state.
    pub state: EvmState,
}

/// Executes or fetches cached values for transactions in a flashblock.
#[derive(Debug)]
pub struct PendingStateBuilder<E, ChainSpec> {
    cumulative_gas_used: u64,
    next_log_index: usize,

    evm: E,
    pending_block: Block<OpTxEnvelope, Header>,
    l1_block_info: L1BlockInfo,
    chain_spec: ChainSpec,
    receipt_builder: OpRethReceiptBuilder,

    prev_pending_blocks: Option<Arc<PendingBlocks>>,
    state_overrides: StateOverride,
}

impl<E, ChainSpec, DB> PendingStateBuilder<E, ChainSpec>
where
    E: Evm<DB = DB>,
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
        receipt_builder: OpRethReceiptBuilder,
    ) -> Self {
        Self {
            pending_block,
            evm,
            cumulative_gas_used: 0,
            next_log_index: 0,
            prev_pending_blocks,
            l1_block_info,
            state_overrides,
            chain_spec,
            receipt_builder,
        }
    }

    /// Consumes the builder and returns the database and state overrides.
    pub fn into_db_and_state_overrides(self) -> (DB, StateOverride) {
        (self.evm.into_db(), self.state_overrides)
    }

    /// Executes a single transaction and updates internal state.
    /// Should be called in order for each transaction.
    pub fn execute_transaction(
        &mut self,
        idx: usize,
        transaction: Recovered<OpTxEnvelope>,
    ) -> Result<ExecutedPendingTransaction> {
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
            Some((receipt, state))
        });

        // If cached, we can fill out pending block data using previous execution results
        // If not cached, we need to execute the transaction and build pending block data from scratch
        if let Some((receipt, state)) = cached_data {
            self.execute_with_cached_data(transaction, receipt, state, idx, effective_gas_price)
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
        idx: usize,
        effective_gas_price: u128,
    ) -> Result<ExecutedPendingTransaction> {
        let (deposit_receipt_version, deposit_nonce) = if transaction.is_deposit() {
            let deposit_receipt = receipt
                .inner
                .inner
                .as_deposit_receipt()
                .ok_or(StateProcessorError::DepositReceiptMismatch)?;

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
            .ok_or(StateProcessorError::GasOverflow)?;
        self.next_log_index += receipt.inner.logs().len();

        Ok(ExecutedPendingTransaction { rpc_transaction, receipt, state })
    }

    /// Executes the transaction through the EVM and builds the result from scratch.
    fn execute_with_evm(
        &mut self,
        transaction: Recovered<OpTxEnvelope>,
        idx: usize,
        effective_gas_price: u128,
    ) -> Result<ExecutedPendingTransaction> {
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
                    .ok_or(StateProcessorError::GasOverflow)?;

                let is_canyon_active =
                    self.chain_spec.is_canyon_active_at_timestamp(self.pending_block.timestamp);

                let is_regolith_active =
                    self.chain_spec.is_regolith_active_at_timestamp(self.pending_block.timestamp);

                let receipt = match self.receipt_builder.build_receipt(ReceiptBuilderCtx {
                    tx: &transaction,
                    evm: &mut self.evm,
                    result,
                    state: &state,
                    cumulative_gas_used: self.cumulative_gas_used,
                }) {
                    Ok(receipt) => receipt,
                    Err(ctx) => {
                        // This is a deposit transaction, so build the receipt from the context
                        let receipt = alloy_consensus::Receipt {
                            status: Eip658Value::Eip658(ctx.result.is_success()),
                            cumulative_gas_used: ctx.cumulative_gas_used,
                            logs: ctx.result.into_logs(),
                        };

                        let deposit_nonce = (is_regolith_active && transaction.is_deposit())
                            .then(|| {
                                self.evm
                                    .db_mut()
                                    .basic(transaction.signer())
                                    .map(|acc| acc.unwrap_or_default().nonce)
                            })
                            .transpose()
                            .map_err(|_| StateProcessorError::DepositAccountLoad)?;

                        self.receipt_builder.build_deposit_receipt(OpDepositReceipt {
                            inner: receipt,
                            deposit_nonce,
                            deposit_receipt_version: is_canyon_active.then_some(1),
                        })
                    }
                };

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

                let op_receipt =
                    OpRpcReceiptBuilder::new(&self.chain_spec, input, &mut self.l1_block_info)
                        .unwrap()
                        .build();
                self.next_log_index += receipt.logs().len();

                let (deposit_receipt_version, deposit_nonce) = if transaction.is_deposit() {
                    let deposit_receipt = op_receipt
                        .inner
                        .inner
                        .as_deposit_receipt()
                        .ok_or(StateProcessorError::DepositReceiptMismatch)?;

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

                Ok(ExecutedPendingTransaction { rpc_transaction, receipt: op_receipt, state })
            }
            Err(e) => Err(StateProcessorError::TransactionExecution {
                tx_hash,
                sender: transaction.signer(),
                reason: format!("{:?}", e),
            }),
        }
    }
}
