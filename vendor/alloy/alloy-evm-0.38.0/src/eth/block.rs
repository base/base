//! Ethereum block executor.

use core::cmp::min;

use super::{
    dao_fork, eip6110,
    receipt_builder::{AlloyReceiptBuilder, ReceiptBuilder, ReceiptBuilderCtx},
    spec::{EthExecutorSpec, EthSpec},
    EthEvmFactory,
};
use crate::{
    block::{
        state_changes::post_block_balance_increments, BlockExecutionError, BlockExecutionResult,
        BlockExecutor, BlockExecutorFactory, BlockValidationError, ExecutableTx, GasOutput,
        StateDB, SystemCaller, TxResult,
    },
    Evm, EvmFactory, FromRecoveredTx, FromTxWithEncoded, RecoveredTx,
};
use alloc::{borrow::Cow, vec::Vec};
use alloy_consensus::{Header, Transaction, TransactionEnvelope, TxReceipt};
use alloy_eips::{eip4895::Withdrawal, eip7685::Requests, Encodable2718};
use alloy_hardforks::EthereumHardfork;
use alloy_primitives::{Bytes, Log, B256};
use revm::{
    context::Block, context_interface::result::ResultAndState, database::DatabaseCommitExt,
    DatabaseCommit, Inspector,
};

/// Context for Ethereum block execution.
#[derive(Debug, Clone)]
pub struct EthBlockExecutionCtx<'a> {
    /// Parent block hash.
    pub parent_hash: B256,
    /// Parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
    /// Block ommers
    pub ommers: &'a [Header],
    /// Block withdrawals.
    pub withdrawals: Option<Cow<'a, [Withdrawal]>>,
    /// Block extra data.
    pub extra_data: Bytes,
    /// Block transactions count hint. Used to preallocate the receipts vector.
    pub tx_count_hint: Option<usize>,
    /// Slot number (EIP-7843, Amsterdam).
    pub slot_number: Option<u64>,
}

/// Block executor for Ethereum.
#[derive(Debug)]
pub struct EthBlockExecutor<'a, Evm, Spec, R: ReceiptBuilder> {
    /// Reference to the specification object.
    pub spec: Spec,

    /// Context for block execution.
    pub ctx: EthBlockExecutionCtx<'a>,
    /// Inner EVM.
    pub evm: Evm,
    /// Utility to call system smart contracts.
    pub system_caller: SystemCaller<Spec>,
    /// Receipt builder.
    pub receipt_builder: R,

    /// Receipts of executed transactions.
    pub receipts: Vec<R::Receipt>,

    /// Cumulative gas used by transactions in this block.
    pub cumulative_tx_gas_used: u64,
    /// Total gas used by transactions in this block.
    pub block_regular_gas_used: u64,
    /// State gas used by transactions in this block.
    pub block_state_gas_used: u64,

    /// Blob gas used by the block.
    /// Before cancun activation, this is always 0.
    pub blob_gas_used: u64,
}

/// The result of executing an Ethereum transaction.
#[derive(Debug)]
pub struct EthTxResult<H, T> {
    /// Result of the transaction execution.
    pub result: ResultAndState<H>,
    /// Blob gas used by the transaction.
    pub blob_gas_used: u64,
    /// Type of the transaction.
    pub tx_type: T,
}

impl<H, T> TxResult for EthTxResult<H, T>
where
    H: Send + 'static,
    T: Send + 'static,
{
    type HaltReason = H;

    fn result(&self) -> &ResultAndState<Self::HaltReason> {
        &self.result
    }

    fn into_result(self) -> ResultAndState<Self::HaltReason> {
        self.result
    }
}

impl<'a, Evm, Spec, R> EthBlockExecutor<'a, Evm, Spec, R>
where
    R: ReceiptBuilder,
{
    /// Creates a new [`EthBlockExecutor`]
    pub fn new(evm: Evm, ctx: EthBlockExecutionCtx<'a>, spec: Spec, receipt_builder: R) -> Self
    where
        Spec: Clone,
    {
        let tx_count_hint = ctx.tx_count_hint.unwrap_or_default();
        Self {
            evm,
            ctx,
            receipts: Vec::with_capacity(tx_count_hint),
            block_regular_gas_used: 0,
            block_state_gas_used: 0,
            cumulative_tx_gas_used: 0,
            blob_gas_used: 0,
            system_caller: SystemCaller::new(spec.clone()),
            spec,
            receipt_builder,
        }
    }

    /// Reserves capacity for at least `tx_count` additional receipts.
    #[inline]
    pub fn reserve(&mut self, tx_count: usize) {
        self.receipts.reserve(tx_count);
    }

    /// Returns the maximum of regular and state gas used by transactions in this block.
    #[inline]
    pub const fn max_block_gas_used(&self) -> u64 {
        if self.block_regular_gas_used > self.block_state_gas_used {
            return self.block_regular_gas_used;
        }
        self.block_state_gas_used
    }
}

impl<E, Spec, R> BlockExecutor for EthBlockExecutor<'_, E, Spec, R>
where
    E: Evm<DB: StateDB, Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>>,
    Spec: EthExecutorSpec,
    R: ReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt<Log = Log>>,
    <R::Transaction as TransactionEnvelope>::TxType: Send + 'static,
{
    type Transaction = R::Transaction;
    type Receipt = R::Receipt;
    type Evm = E;
    type Result = EthTxResult<E::HaltReason, <R::Transaction as TransactionEnvelope>::TxType>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.system_caller.apply_blockhashes_contract_call(self.ctx.parent_hash, &mut self.evm)?;
        self.system_caller
            .apply_beacon_root_contract_call(self.ctx.parent_beacon_block_root, &mut self.evm)?;

        Ok(())
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        let (tx_env, tx) = tx.into_parts();

        // The sum of the transaction's gas limit, Tg, and the gas utilized in this block prior,
        // must be no greater than the block's gasLimit.
        //
        // Pre-Amsterdam: use tx_gas_used (gas after refunds) as cumulative gas, matching
        // the original behavior where gas_used = spent - refunded.
        //
        // Amsterdam+: use block_regular_gas_used.
        let block_gas_used = if self.evm.cfg_env().enable_amsterdam_eip8037 {
            self.block_regular_gas_used
        } else {
            self.cumulative_tx_gas_used
        };
        let block_available_gas = self.evm.block().gas_limit() - block_gas_used;

        // Use regular part of transaction gas limit to check if it fits inside available block
        // space.
        let mut max_tx_gas_usage = tx.tx().gas_limit();
        if let Some(tx_gas_limit_cap) = self.evm.cfg_env().tx_gas_limit_cap {
            max_tx_gas_usage = min(max_tx_gas_usage, tx_gas_limit_cap);
        }

        if max_tx_gas_usage > block_available_gas {
            return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: tx.tx().gas_limit(),
                block_available_gas,
            }
            .into());
        }

        // Execute transaction and return the result
        let result = self.evm.transact(tx_env).map_err(|err| {
            let hash = tx.tx().trie_hash();
            BlockExecutionError::evm(err, hash)
        })?;

        Ok(EthTxResult {
            result,
            blob_gas_used: tx.tx().blob_gas_used().unwrap_or_default(),
            tx_type: tx.tx().tx_type(),
        })
    }

    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        let EthTxResult { result: ResultAndState { result, state }, blob_gas_used, tx_type } =
            output;

        let tx_gas_used = result.gas().tx_gas_used();
        let regular_gas_used = result.gas().block_regular_gas_used();
        let state_gas_used = result.gas().block_state_gas_used();

        // append used gas used
        self.block_regular_gas_used += regular_gas_used;
        self.block_state_gas_used += state_gas_used;
        self.cumulative_tx_gas_used += tx_gas_used;

        // only determine cancun fields when active
        if self.spec.is_cancun_active_at_timestamp(self.evm.block().timestamp().saturating_to()) {
            self.blob_gas_used = self.blob_gas_used.saturating_add(blob_gas_used);
        }

        // Push transaction changeset and calculate header bloom filter for receipt.
        self.receipts.push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
            tx_type,
            evm: &self.evm,
            result,
            state: &state,
            cumulative_gas_used: self.cumulative_tx_gas_used,
        }));

        // Commit the state changes.
        self.evm.db_mut().commit(state);

        GasOutput::with_state_gas(tx_gas_used, state_gas_used)
    }

    fn finish(
        mut self,
    ) -> Result<(Self::Evm, BlockExecutionResult<R::Receipt>), BlockExecutionError> {
        let requests = if self
            .spec
            .is_prague_active_at_timestamp(self.evm.block().timestamp().saturating_to())
        {
            // Collect all EIP-6110 deposits
            let deposit_requests =
                eip6110::parse_deposits_from_receipts(&self.spec, &self.receipts)?;

            let mut requests = Requests::default();
            if !deposit_requests.is_empty() {
                requests.push_request_with_type(eip6110::DEPOSIT_REQUEST_TYPE, deposit_requests);
            }

            self.system_caller.append_post_execution_changes(&mut self.evm, &mut requests)?;
            requests
        } else {
            Requests::default()
        };

        let mut balance_increments = post_block_balance_increments(
            &self.spec,
            self.evm.block(),
            self.ctx.ommers,
            self.ctx.withdrawals.as_deref(),
        );

        // Irregular state change at Ethereum DAO hardfork
        if self
            .spec
            .ethereum_fork_activation(EthereumHardfork::Dao)
            .transitions_at_block(self.evm.block().number().saturating_to())
        {
            // drain balances from hardcoded addresses.
            let drained_balance: u128 = self
                .evm
                .db_mut()
                .drain_balances(dao_fork::DAO_HARDFORK_ACCOUNTS)
                .map_err(|_| BlockValidationError::IncrementBalanceFailed)?
                .into_iter()
                .sum();

            // return balance to DAO beneficiary.
            *balance_increments.entry(dao_fork::DAO_HARDFORK_BENEFICIARY).or_default() +=
                drained_balance;
        }
        // increment balances
        self.evm
            .db_mut()
            .increment_balances(balance_increments)
            .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;

        // Pre-Amsterdam: use tx_gas_used (with refunds) for the block gas total.
        // Amsterdam+: use max(regular, state) gas without refunds (EIP-8037).
        let gas_used = if self.evm.cfg_env().enable_amsterdam_eip8037 {
            self.max_block_gas_used()
        } else {
            self.cumulative_tx_gas_used
        };

        Ok((
            self.evm,
            BlockExecutionResult {
                receipts: self.receipts,
                requests,
                gas_used,
                blob_gas_used: self.blob_gas_used,
            },
        ))
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    fn evm(&self) -> &Self::Evm {
        &self.evm
    }

    fn receipts(&self) -> &[Self::Receipt] {
        &self.receipts
    }
}

/// Ethereum block executor factory.
#[derive(Debug, Clone, Default, Copy)]
pub struct EthBlockExecutorFactory<
    R = AlloyReceiptBuilder,
    Spec = EthSpec,
    EvmFactory = EthEvmFactory,
> {
    /// Receipt builder.
    receipt_builder: R,
    /// Chain specification.
    spec: Spec,
    /// EVM factory.
    evm_factory: EvmFactory,
}

impl<R, Spec, EvmFactory> EthBlockExecutorFactory<R, Spec, EvmFactory> {
    /// Creates a new [`EthBlockExecutorFactory`] with the given spec, [`EvmFactory`], and
    /// [`ReceiptBuilder`].
    pub const fn new(receipt_builder: R, spec: Spec, evm_factory: EvmFactory) -> Self {
        Self { receipt_builder, spec, evm_factory }
    }

    /// Exposes the receipt builder.
    pub const fn receipt_builder(&self) -> &R {
        &self.receipt_builder
    }

    /// Exposes the chain specification.
    pub const fn spec(&self) -> &Spec {
        &self.spec
    }

    /// Exposes the EVM factory.
    pub const fn evm_factory(&self) -> &EvmFactory {
        &self.evm_factory
    }
}

impl<R, Spec, EvmF> BlockExecutorFactory for EthBlockExecutorFactory<R, Spec, EvmF>
where
    R: ReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt<Log = Log>>,
    Spec: EthExecutorSpec,
    EvmF: EvmFactory<Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>>,
    <R::Transaction as TransactionEnvelope>::TxType: Send + 'static,
    Self: 'static,
{
    type EvmFactory = EvmF;
    type ExecutionCtx<'a> = EthBlockExecutionCtx<'a>;
    type Transaction = R::Transaction;
    type Receipt = R::Receipt;
    type TxExecutionResult = EthTxResult<
        <EvmF as EvmFactory>::HaltReason,
        <R::Transaction as TransactionEnvelope>::TxType,
    >;
    type Executor<'a, DB: StateDB, I: Inspector<EvmF::Context<DB>>> =
        EthBlockExecutor<'a, EvmF::Evm<DB, I>, &'a Spec, &'a R>;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: EvmF::Evm<DB, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a, DB, I>
    where
        DB: StateDB,
        I: Inspector<EvmF::Context<DB>>,
    {
        EthBlockExecutor::new(evm, ctx, &self.spec, &self.receipt_builder)
    }
}
