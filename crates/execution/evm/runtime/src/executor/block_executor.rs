//! Contains the block executor for base.

use alloc::{boxed::Box, vec::Vec};

use alloy_eips::{Encodable2718, Typed2718};
use base_common_chain_config::{Upgrades, tx_estimated_size_fjord as estimate_tx_compressed_size};
use base_common_types_chain::{
    BaseReceipt, BaseTxEnvelope, DepositReceipt, Eip658Value, Eip8130Receipt, Header, OpTxType,
    Predeploys, Transaction, TransactionEnvelope, TxReceipt,
};
#[cfg(feature = "std")]
use base_execution_evm_runtime::IntrinsicGas;
use base_execution_evm_runtime::{
    BalIndexedDatabase, BaseBlockExecutionCtx, BaseBlockExecutionError, BaseTime, BaseTransaction,
    BaseTxResult, Block, BlockExecutionError, BlockExecutionResult, BlockValidationError,
    CommitChanges, DEPOSIT_TRANSACTION_TYPE, Database, DatabaseCommit, DatabaseCommitExt, Evm,
    ExecutableTx, GasOutput, L1BlockInfo, RecoveredTx, ResultAndState, StateDB, SystemCaller,
    canyon, post_block_balance_increments,
};

/// Block executor for Base.
pub struct BaseBlockExecutor<DB: Database, I> {
    /// Spec.
    pub spec: base_common_chain_config::ChainConfig,
    /// Context for block execution.
    pub ctx: BaseBlockExecutionCtx,
    /// The EVM used by executor.
    pub evm: crate::BaseEvm<DB, I>,
    /// Receipts of executed transactions.
    pub receipts: Vec<BaseReceipt>,
    /// Total gas used by executed transactions.
    pub gas_used: u64,
    /// DA footprint.
    ///
    /// This is only set for blocks post-Jovian activation.
    /// See [DA footprint block limit spec](https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/jovian/exec-engine.md#da-footprint-block-limit)
    pub da_footprint_used: u64,
    /// Whether Regolith upgrade is active.
    pub is_regolith: bool,
    /// Utility to call system smart contracts.
    pub system_caller: SystemCaller,
}

impl<DB: Database, I> core::fmt::Debug for BaseBlockExecutor<DB, I> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BaseBlockExecutor")
            .field("spec", &self.spec)
            .field("ctx", &self.ctx)
            .field("receipts", &self.receipts)
            .field("gas_used", &self.gas_used)
            .field("da_footprint_used", &self.da_footprint_used)
            .finish_non_exhaustive()
    }
}

impl<DB, I> BaseBlockExecutor<DB, I>
where
    DB: StateDB,
    I: crate::Inspector<crate::BaseContext<DB>>,
{
    /// Creates a new [`BaseBlockExecutor`].
    pub fn new(
        evm: crate::BaseEvm<DB, I>,
        ctx: BaseBlockExecutionCtx,
        spec: base_common_chain_config::ChainConfig,
    ) -> Self {
        Self {
            is_regolith: spec
                .is_regolith_active_at_timestamp(evm.block().timestamp().saturating_to()),
            evm,
            system_caller: SystemCaller::new(spec.clone()),
            spec,
            receipts: Vec::new(),
            gas_used: 0,
            da_footprint_used: 0,
            ctx,
        }
    }
}

impl<DB, I> BaseBlockExecutor<DB, I>
where
    DB: StateDB,
    I: crate::Inspector<crate::BaseContext<DB>>,
{
    /// Block gas the transaction may consume, reserved against the block gas
    /// limit before execution.
    ///
    /// For every transaction this is the declared `gas_limit`. An EIP-8130
    /// transaction additionally meters `payer_auth` *on top of* `gas_limit` (the
    /// payer reimburses its own authentication beyond the sender-signed limit),
    /// so that portion must be reserved in the block gas budget too. Reserving
    /// `gas_limit` alone could admit a transaction whose true consumption
    /// (`gas_limit + payer_auth`) pushes cumulative block gas over the limit.
    ///
    /// The payer authentication gas is a *conservative upper bound*
    /// ([`IntrinsicGas::max_payer_auth_cost`]): it pins the payer's policy gate
    /// worst-case, since the pre-execution check cannot resolve the payer's
    /// on-chain scope. Reserving a ceiling can only over-reserve (never admit an
    /// over-limit block), and the same bound is used by block building and
    /// validation, keeping them consistent.
    #[cfg(feature = "std")]
    fn reserved_block_gas(
        tx_env: &BaseTransaction,
        gas_limit: u64,
    ) -> Result<u64, BlockExecutionError> {
        let Some(signed) = tx_env.eip8130.as_ref().map(|parts| &parts.signed) else {
            return Ok(gas_limit);
        };
        let payer_auth =
            IntrinsicGas::max_payer_auth_cost(signed).map_err(BlockExecutionError::other)?;
        Ok(gas_limit.saturating_add(payer_auth))
    }

    /// `no_std` builds reject EIP-8130 execution outright (see `Evm::transact_raw`),
    /// so no payer authentication gas is metered on top of `gas_limit` and the
    /// reserved block gas is just the declared `gas_limit`.
    #[cfg(not(feature = "std"))]
    const fn reserved_block_gas(
        _tx_env: &BaseTransaction,
        gas_limit: u64,
    ) -> Result<u64, BlockExecutionError> {
        Ok(gas_limit)
    }

    fn jovian_da_footprint_estimation(
        &mut self,
        tx_env: &BaseTransaction,
        tx: impl RecoveredTx<BaseTxEnvelope>,
    ) -> Result<u64, BlockExecutionError> {
        // Try to use the enveloped tx if it exists, otherwise use the encoded 2718 bytes
        let encoded = tx_env
            .enveloped_tx
            .as_ref()
            .map_or_else(
                || estimate_tx_compressed_size(tx.tx().encoded_2718().as_ref()),
                |encoded| estimate_tx_compressed_size(encoded),
            )
            .saturating_div(1_000_000);

        // Load the L1 block contract into the cache. If the L1 block contract is not pre-loaded the
        // database will panic when trying to fetch the DA footprint gas scalar.
        self.evm.db_mut().basic(Predeploys::L1_BLOCK_INFO).map_err(BlockExecutionError::other)?;

        let da_footprint_gas_scalar = L1BlockInfo::fetch_da_footprint_gas_scalar(self.evm.db_mut())
            .map_err(BlockExecutionError::other)?
            .into();

        Ok(encoded.saturating_mul(da_footprint_gas_scalar))
    }
}

impl<DB, I> BaseBlockExecutor<DB, I>
where
    DB: StateDB,
    I: crate::Inspector<crate::BaseContext<DB>>,
{
    /// Applies any necessary changes before executing the block's transactions.
    pub fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.system_caller.apply_blockhashes_contract_call(self.ctx.parent_hash, &mut self.evm)?;
        self.system_caller
            .apply_beacon_root_contract_call(self.ctx.parent_beacon_block_root, &mut self.evm)?;

        // Ensure that the create2deployer is force-deployed at the canyon transition. Base
        // blocks will always have at least a single transaction in them (the L1 info transaction),
        // so we can safely assume that this will always be triggered upon the transition and that
        // the above check for empty blocks will never be hit on Base chains.
        canyon::ensure_create2_deployer(
            &self.spec,
            self.evm.block().timestamp().saturating_to(),
            self.evm.db_mut(),
        )
        .map_err(BlockExecutionError::other)?;

        // Install BaseTime before the Cobalt system-account transition and transactions so the
        // activation block's metadata deposit can call it.
        BaseTime::ensure_predeploy(
            &self.spec,
            self.evm.block().timestamp().saturating_to(),
            self.evm.db_mut(),
        )
        .map_err(BlockExecutionError::other)?;

        // At the Zenith (EIP-8130) transition, plant a code stub on the code-less
        // enshrined system accounts (the 2D nonce manager) so the persistent state
        // the enshrined path writes to them is not reaped by EIP-161 end-of-block
        // state clearing.
        crate::zenith::ensure_eip8130_system_accounts(
            &self.spec,
            self.evm.block().timestamp().saturating_to(),
            self.evm.db_mut(),
        )
        .map_err(BlockExecutionError::other)?;

        Ok(())
    }

    /// Executes a single transaction without committing state changes.
    ///
    /// This method performs the transaction execution through the EVM but does not
    /// commit the resulting state changes. The output can be inspected and potentially
    /// committed later using [`commit_transaction`](Self::commit_transaction).
    ///
    /// Returns a [`base_execution_evm_runtime::ResultAndState`] containing the execution
    /// result and state changes.
    ///
    /// # Use Cases
    /// - Transaction simulation without affecting state
    /// - Inspecting transaction effects before committing
    /// - Building custom commit logic
    pub fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx,
    ) -> Result<BaseTxResult, BlockExecutionError> {
        let (tx_env, tx) = tx.into_parts();
        let is_deposit = tx.tx().ty() == DEPOSIT_TRANSACTION_TYPE;

        // The sum of the gas the transaction may consume, Tg, and the gas utilized in this block
        // prior, must be no greater than the block's gasLimit. For EIP-8130 the reserved amount is
        // `gas_limit + payer_auth`, since payer authentication is metered on top of the declared
        // gas_limit (see `reserved_block_gas`); for every other transaction it is `gas_limit`.
        let reserved_gas = Self::reserved_block_gas(&tx_env, tx.tx().gas_limit())?;
        let block_available_gas = self.evm.block().gas_limit().saturating_sub(self.gas_used);
        if reserved_gas > block_available_gas && (self.is_regolith || !is_deposit) {
            return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: reserved_gas,
                block_available_gas,
            }
            .into());
        }

        let da_footprint_used = if self
            .spec
            .is_jovian_active_at_timestamp(self.evm.block().timestamp().saturating_to())
            && !is_deposit
        {
            let da_footprint_available =
                self.evm.block().gas_limit().saturating_sub(self.da_footprint_used);

            let tx_da_footprint = self.jovian_da_footprint_estimation(&tx_env, &tx)?;

            if tx_da_footprint > da_footprint_available {
                return Err(BlockExecutionError::Validation(BlockValidationError::Other(
                    Box::new(BaseBlockExecutionError::TransactionDaFootprintAboveGasLimit {
                        transaction_da_footprint: tx_da_footprint,
                        available_block_da_footprint: da_footprint_available,
                    }),
                )));
            }

            tx_da_footprint
        } else {
            0
        };

        // Execute transaction and return the result
        let result = self.evm.transact(tx_env).map_err(|err| {
            let hash = tx.tx().trie_hash();
            BlockExecutionError::evm(err, hash)
        })?;

        // Fetch the depositor account from the database for the deposit nonce.
        // This *only* needs to be done post-Regolith for deposit transactions.
        let depositor = (self.is_regolith && is_deposit)
            .then(|| self.evm.db_mut().basic(*tx.signer()).map(|acc| acc.unwrap_or_default()))
            .transpose()
            .map_err(BlockExecutionError::other)?;

        Ok(BaseTxResult {
            result,
            blob_gas_used: da_footprint_used,
            tx_type: tx.tx().tx_type(),
            is_deposit,
            sender: *tx.signer(),
            depositor,
        })
    }

    /// Commits a previously executed transaction's state changes.
    ///
    /// Takes the output from
    /// [`execute_transaction_without_commit`](Self::execute_transaction_without_commit)
    /// and applies the state changes, updates gas accounting, and generates a receipt.
    ///
    /// Returns the gas used by the transaction (including both regular and state gas).
    ///
    /// # Parameters
    /// - `output`: The transaction output containing execution result and state changes
    pub fn commit_transaction(&mut self, output: BaseTxResult) -> GasOutput {
        let BaseTxResult {
            result: ResultAndState { result, state },
            blob_gas_used,
            tx_type,
            is_deposit,
            sender: _,
            depositor,
        } = output;

        let tx_gas_used = result.tx_gas_used();
        let state_gas_used = result.gas().block_state_gas_used();

        self.gas_used += tx_gas_used;

        if self.spec.is_jovian_active_at_timestamp(self.evm.block().timestamp().saturating_to())
            && !is_deposit
        {
            self.da_footprint_used = self.da_footprint_used.saturating_add(blob_gas_used);
        }

        let receipt = base_common_types_chain::Receipt {
            status: Eip658Value::Eip658(result.is_success()),
            cumulative_gas_used: self.gas_used,
            logs: result.into_logs(),
        };
        self.receipts.push(match tx_type {
            OpTxType::Legacy => BaseReceipt::Legacy(receipt),
            OpTxType::Eip2930 => BaseReceipt::Eip2930(receipt),
            OpTxType::Eip1559 => BaseReceipt::Eip1559(receipt),
            OpTxType::Eip7702 => BaseReceipt::Eip7702(receipt),
            OpTxType::Eip8130 => BaseReceipt::Eip8130(Eip8130Receipt::new(
                receipt,
                crate::Eip8130PhaseStatuses::take(),
            )),
            OpTxType::Deposit => BaseReceipt::Deposit(DepositReceipt {
                inner: receipt,
                deposit_nonce: depositor.map(|account| account.nonce),
                deposit_receipt_version: self
                    .spec
                    .is_canyon_active_at_timestamp(self.evm.block().timestamp().saturating_to())
                    .then_some(1),
            }),
        });

        self.evm.db_mut().commit(state);

        GasOutput::with_state_gas(tx_gas_used, state_gas_used)
    }

    /// Applies any necessary changes after executing the block's transactions, completes execution
    /// and returns the underlying EVM along with execution result.
    pub fn finish(
        mut self,
    ) -> Result<(crate::BaseEvm<DB, I>, BlockExecutionResult<BaseReceipt>), BlockExecutionError>
    {
        let balance_increments =
            post_block_balance_increments::<Header>(&self.spec, self.evm.block(), &[], None);

        self.evm
            .db_mut()
            .increment_balances(balance_increments)
            .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;

        let legacy_gas_used =
            self.receipts.last().map(|r| r.cumulative_gas_used()).unwrap_or_default();

        Ok((
            self.evm,
            BlockExecutionResult {
                receipts: self.receipts,
                requests: Default::default(),
                gas_used: legacy_gas_used,
                blob_gas_used: self.da_footprint_used,
            },
        ))
    }

    /// Exposes mutable reference to EVM.
    pub fn evm_mut(&mut self) -> &mut crate::BaseEvm<DB, I> {
        &mut self.evm
    }

    /// Exposes immutable reference to EVM.
    pub fn evm(&self) -> &crate::BaseEvm<DB, I> {
        &self.evm
    }

    /// Returns a reference to all recorded receipts.
    pub fn receipts(&self) -> &[BaseReceipt] {
        &self.receipts
    }

    /// Executes a single transaction and applies execution result to internal state.
    ///
    /// This method accepts any type implementing [`ExecutableTx`], which ensures the transaction:
    /// - Can be converted to the EVM's transaction environment for execution
    /// - Provides access to the original transaction and signer for receipt generation
    ///
    /// Common input types include:
    /// - `&Recovered<Transaction>` - A transaction with its recovered sender
    /// - `&WithEncoded<Recovered<Transaction>>` - A transaction with sender and encoded bytes
    ///
    /// The transaction is executed in the EVM, state changes are committed, and a receipt
    /// is generated internally.
    ///
    /// Returns the gas used by the transaction.
    pub fn execute_transaction(
        &mut self,
        tx: impl ExecutableTx,
    ) -> Result<GasOutput, BlockExecutionError> {
        self.execute_transaction_with_result_closure(tx, |_| ())
    }

    /// Executes a single transaction at its zero-based index in the block and applies execution
    /// result to internal state.
    ///
    /// This is equivalent to [`execute_transaction`](Self::execute_transaction), but first sets
    /// the underlying database's BAL index for the transaction's position in the block. BAL uses
    /// index `0` for pre-execution changes, transaction indexes are shifted by one (`tx_index + 1`)
    /// and post-execution changes use the index after the last transaction. This means transaction
    /// `0` is executed at BAL index `1`, transaction `1` at BAL index `2`, and so on.
    ///
    /// Callers that execute transactions individually should prefer this method when the
    /// transaction index in the block is known, so BAL reads and writes are attributed to the
    /// correct EIP-7928 index.
    pub fn execute_transaction_with_index(
        &mut self,
        tx: impl ExecutableTx,
        tx_index: usize,
    ) -> Result<GasOutput, BlockExecutionError>
    where
        DB: BalIndexedDatabase,
    {
        self.execute_transaction_with_index_and_result_closure(tx, tx_index, |_| ())
    }

    /// Executes a single transaction at its zero-based index in the block and invokes the given
    /// closure with the internal [`BaseTxResult`](BaseTxResult) produced by the EVM.
    ///
    /// This is the indexed counterpart to
    /// [`execute_transaction_with_result_closure`](Self::execute_transaction_with_result_closure).
    /// It first sets the underlying database's BAL index for the transaction's position in the
    /// block, then executes and commits the transaction while exposing the raw execution result to
    /// the closure.
    ///
    /// BAL uses index `0` for pre-execution changes, transaction indexes are shifted by one
    /// (`tx_index + 1`) and post-execution changes use the index after the last transaction. This
    /// means transaction `0` is executed at BAL index `1`, transaction `1` at BAL index `2`, and so
    /// on.
    pub fn execute_transaction_with_index_and_result_closure(
        &mut self,
        tx: impl ExecutableTx,
        tx_index: usize,
        f: impl FnOnce(&BaseTxResult),
    ) -> Result<GasOutput, BlockExecutionError>
    where
        DB: BalIndexedDatabase,
    {
        self.evm_mut().db_mut().set_bal_index(tx_index as u64 + 1);
        self.execute_transaction_with_result_closure(tx, f)
    }

    /// Executes a single transaction and applies execution result to internal state. Invokes the
    /// given closure with an internal [`BaseTxResult`](BaseTxResult) produced by the EVM.
    ///
    /// This method is similar to [`execute_transaction`](Self::execute_transaction) but provides
    /// access to the raw execution result before it's converted to a receipt. This is useful for:
    /// - Custom logging or metrics collection
    /// - Debugging transaction execution
    /// - Extracting additional information from the execution result
    ///
    /// The transaction is always committed after the closure is invoked.
    pub fn execute_transaction_with_result_closure(
        &mut self,
        tx: impl ExecutableTx,
        f: impl FnOnce(&BaseTxResult),
    ) -> Result<GasOutput, BlockExecutionError> {
        self.execute_transaction_with_commit_condition(tx, |res| {
            f(res);
            CommitChanges::Yes
        })
        .map(Option::unwrap_or_default)
    }

    /// Executes a single transaction and applies execution result to internal state. Invokes the
    /// given closure with an internal [`BaseTxResult`](BaseTxResult) produced by the EVM,
    /// and commits the transaction to the state on [`CommitChanges::Yes`].
    ///
    /// This is the most flexible transaction execution method, allowing conditional commitment
    /// based on the execution result. The closure receives the execution result and returns
    /// whether to commit the changes to state.
    ///
    /// Use cases:
    /// - Conditional execution based on transaction outcome
    /// - Simulating transactions without committing
    /// - Custom validation logic before committing
    ///
    /// The [`ExecutableTx`] constraint ensures that:
    /// 1. The transaction can be converted to `TxEnv` via [`ToTxEnv`] for EVM execution
    /// 2. The original transaction and signer can be accessed via [`RecoveredTx`] for receipt
    ///    generation
    ///
    /// Returns [`None`] if committing changes from the transaction should be skipped via
    /// [`CommitChanges::No`], otherwise returns the gas used by the transaction.
    pub fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutableTx,
        f: impl FnOnce(&BaseTxResult) -> CommitChanges,
    ) -> Result<Option<GasOutput>, BlockExecutionError> {
        // Execute transaction without committing
        let output = self.execute_transaction_without_commit(tx)?;

        if !f(&output).should_commit() {
            return Ok(None);
        }

        let gas_used = self.commit_transaction(output);
        Ok(Some(gas_used))
    }

    /// A helper to invoke [`base_execution_evm_runtime::BaseBlockExecutor::finish`] returning only the [`BlockExecutionResult`].
    pub fn apply_post_execution_changes(
        self,
    ) -> Result<BlockExecutionResult<BaseReceipt>, BlockExecutionError>
    where
        Self: Sized,
    {
        self.finish().map(|(_, result)| result)
    }

    /// Executes all transactions in a block, applying pre and post execution changes.
    ///
    /// This is a convenience method that orchestrates the complete block execution flow:
    /// 1. Applies pre-execution changes (system calls, irregular state transitions)
    /// 2. Executes all transactions in order
    /// 3. Applies post-execution changes (block rewards, system calls)
    ///
    /// Each transaction in the iterator must implement [`ExecutableTx`], ensuring it can be:
    /// - Converted to the EVM's transaction format for execution
    /// - Used to generate receipts with access to the original transaction data
    ///
    /// # Example
    ///
    /// ```ignore
    /// let recovered_txs: Vec<Recovered<Transaction>> = block.transactions
    ///     .iter()
    ///     .map(|tx| tx.recover_signer())
    ///     .collect::<Result<_, _>>()?;
    ///
    /// let result = executor.execute_block(recovered_txs.iter())?;
    /// ```
    pub fn execute_block(
        mut self,
        transactions: impl IntoIterator<Item = impl ExecutableTx>,
    ) -> Result<BlockExecutionResult<BaseReceipt>, BlockExecutionError>
    where
        Self: Sized,
    {
        self.apply_pre_execution_changes()?;

        for tx in transactions {
            self.execute_transaction(tx)?;
        }

        self.apply_post_execution_changes()
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec};

    use alloy_eips::eip2718::WithEncoded;
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::{Address, Bytes, Signature, U256, uint};
    use base_common_chain_config::{BaseUpgrade, ChainUpgrades};
    use base_common_types_chain::{
        BaseTxEnvelope, Eip8130Constants, Eip8130Signed, Predeploys, SignableTransaction,
        TxEip8130, TxLegacy, transaction::Recovered,
    };
    use base_execution_evm_runtime::{
        AccountInfo, BaseBlockExecutorFactory, BaseEvmFactory, BaseSpecId, BlockEnv, Builder,
        CacheDB, Context, DefaultBase, EmptyDB, EvmEnv, HashMap, InMemoryDB, L1BlockInfo,
        NoOpInspector, ToTxEnv,
    };

    use super::*;

    #[test]
    fn test_with_encoded() {
        let executor_factory = BaseBlockExecutorFactory::new(
            base_common_chain_config::ChainConfig::mainnet().clone(),
            BaseEvmFactory::default(),
        );
        let mut db = base_execution_evm_runtime::State::builder()
            .with_database(CacheDB::<EmptyDB>::default())
            .build();
        let evm = executor_factory.evm_factory.create_evm(&mut db, EvmEnv::default());
        let mut executor = executor_factory.create_executor(evm, BaseBlockExecutionCtx::default());
        let tx = Recovered::new_unchecked(
            BaseTxEnvelope::Legacy(TxLegacy::default().into_signed(Signature::new(
                Default::default(),
                Default::default(),
                Default::default(),
            ))),
            Address::ZERO,
        );
        let tx_with_encoded = WithEncoded::new(tx.encoded_2718().into(), tx.clone());

        // make sure we can use both `WithEncoded` and transaction itself as inputs.
        let _ = executor.execute_transaction(&tx);
        let _ = executor.execute_transaction(&tx_with_encoded);
    }

    fn prepare_jovian_db(
        da_footprint_gas_scalar: u16,
    ) -> base_execution_evm_runtime::State<InMemoryDB> {
        const L1_BASE_FEE: U256 = uint!(1_U256);
        const L1_BLOB_BASE_FEE: U256 = uint!(2_U256);
        const L1_BASE_FEE_SCALAR: u64 = 3;
        const L1_BLOB_BASE_FEE_SCALAR: u64 = 4;
        const L1_FEE_SCALARS: U256 = U256::from_limbs([
            0,
            (L1_BASE_FEE_SCALAR << (64 - L1BlockInfo::BASE_FEE_SCALAR_OFFSET * 2))
                | L1_BLOB_BASE_FEE_SCALAR,
            0,
            0,
        ]);
        const OPERATOR_FEE_SCALAR: u8 = 5;
        const OPERATOR_FEE_CONST: u8 = 6;
        let da_footprint_gas_scalar_bytes = da_footprint_gas_scalar.to_be_bytes();
        let mut operator_fee_and_da_footprint = [0u8; 32];
        operator_fee_and_da_footprint[31] = OPERATOR_FEE_CONST;
        operator_fee_and_da_footprint[23] = OPERATOR_FEE_SCALAR;
        operator_fee_and_da_footprint[19] = da_footprint_gas_scalar_bytes[1];
        operator_fee_and_da_footprint[18] = da_footprint_gas_scalar_bytes[0];
        let operator_fee_and_da_footprint_u256 = U256::from_be_bytes(operator_fee_and_da_footprint);

        let mut db = base_execution_evm_runtime::State::builder()
            .with_database(InMemoryDB::default())
            .build();

        db.insert_account_with_storage(
            Predeploys::L1_BLOCK_INFO,
            Default::default(),
            HashMap::from_iter([
                (L1BlockInfo::L1_BASE_FEE_SLOT, L1_BASE_FEE),
                (L1BlockInfo::ECOTONE_L1_FEE_SCALARS_SLOT, L1_FEE_SCALARS),
                (L1BlockInfo::ECOTONE_L1_BLOB_BASE_FEE_SLOT, L1_BLOB_BASE_FEE),
                (L1BlockInfo::OPERATOR_FEE_SCALARS_SLOT, operator_fee_and_da_footprint_u256),
            ]),
        );

        db.insert_account(
            Address::ZERO,
            AccountInfo { balance: U256::from(400_000_000), ..Default::default() },
        );

        db
    }

    fn build_executor<'a>(
        db: &'a mut base_execution_evm_runtime::State<InMemoryDB>,
        base_chain_upgrades: &'a ChainUpgrades,
        gas_limit: u64,
        jovian_timestamp: u64,
    ) -> BaseBlockExecutor<&'a mut base_execution_evm_runtime::State<InMemoryDB>, NoOpInspector>
    {
        let ctx = Context::base()
            .with_db(db)
            .with_chain(L1BlockInfo {
                operator_fee_scalar: Some(U256::from(2)),
                operator_fee_constant: Some(U256::from(50)),
                ..Default::default()
            })
            .with_block(BlockEnv {
                timestamp: U256::from(jovian_timestamp),
                gas_limit,
                ..Default::default()
            })
            .modify_cfg_chained(|cfg| cfg.spec = BaseSpecId::new(BaseUpgrade::Jovian));

        let evm = ctx.build_with_inspector(NoOpInspector {});

        BaseBlockExecutor::new(
            evm,
            BaseBlockExecutionCtx::default(),
            base_common_chain_config::ChainConfig {
                upgrades: base_chain_upgrades.clone(),
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_jovian_da_footprint_estimation() {
        const DA_FOOTPRINT_GAS_SCALAR: u16 = 7;
        const GAS_LIMIT: u64 = 100_000;
        const JOVIAN_TIMESTAMP: u64 = 1746806402;

        let mut db = prepare_jovian_db(DA_FOOTPRINT_GAS_SCALAR);
        let base_chain_upgrades = ChainUpgrades::new(
            BaseUpgrade::mainnet()
                .into_iter()
                .chain(vec![(BaseUpgrade::Jovian, ForkCondition::Timestamp(JOVIAN_TIMESTAMP))]),
        );

        let mut executor =
            build_executor(&mut db, &base_chain_upgrades, GAS_LIMIT, JOVIAN_TIMESTAMP);

        let tx_inner = TxLegacy { gas_limit: GAS_LIMIT, ..Default::default() };

        let tx = Recovered::new_unchecked(
            BaseTxEnvelope::Legacy(tx_inner.into_signed(Signature::new(
                Default::default(),
                Default::default(),
                Default::default(),
            ))),
            Address::ZERO,
        );
        let tx_env = tx.to_tx_env();

        assert!(executor.da_footprint_used == 0);

        let expected_da_footprint = executor.jovian_da_footprint_estimation(&tx_env, &tx).unwrap();

        // make sure we can use both `WithEncoded` and transaction itself as inputs.
        let res = executor.execute_transaction(&tx);
        assert!(res.is_ok());

        assert!(executor.da_footprint_used == expected_da_footprint);
    }

    #[test]
    fn test_jovian_da_footprint_estimation_out_of_gas() {
        const DA_FOOTPRINT_GAS_SCALAR: u16 = 7;
        const JOVIAN_TIMESTAMP: u64 = 1746806402;
        const GAS_LIMIT: u64 = 100;

        let mut db = prepare_jovian_db(DA_FOOTPRINT_GAS_SCALAR);
        let base_chain_upgrades = ChainUpgrades::new(
            BaseUpgrade::mainnet()
                .into_iter()
                .chain(vec![(BaseUpgrade::Jovian, ForkCondition::Timestamp(JOVIAN_TIMESTAMP))]),
        );

        let mut executor =
            build_executor(&mut db, &base_chain_upgrades, GAS_LIMIT, JOVIAN_TIMESTAMP);

        let tx_inner = TxLegacy { gas_limit: GAS_LIMIT, ..Default::default() };

        let tx = Recovered::new_unchecked(
            BaseTxEnvelope::Legacy(tx_inner.into_signed(Signature::new(
                Default::default(),
                Default::default(),
                Default::default(),
            ))),
            Address::ZERO,
        );
        let tx_env = tx.to_tx_env();

        assert!(executor.da_footprint_used == 0);

        let expected_da_footprint = executor.jovian_da_footprint_estimation(&tx_env, &tx).unwrap();

        // make sure we can use both `WithEncoded` and transaction itself as inputs.
        let res = executor.execute_transaction(&tx);
        assert!(res.is_err());
        let err = res.unwrap_err();
        match err {
            BlockExecutionError::Validation(BlockValidationError::Other(err)) => {
                assert_eq!(
                    err.to_string(),
                    BaseBlockExecutionError::TransactionDaFootprintAboveGasLimit {
                        transaction_da_footprint: expected_da_footprint,
                        available_block_da_footprint: GAS_LIMIT,
                    }
                    .to_string(),
                );
            }
            _ => panic!("expected TransactionDaFootprintAboveGasLimit error"),
        }
    }

    #[test]
    fn test_jovian_da_footprint_estimation_maxed_out_da_footprint() {
        const DA_FOOTPRINT_GAS_SCALAR: u16 = 2000;
        const JOVIAN_TIMESTAMP: u64 = 1746806402;
        const GAS_LIMIT: u64 = 200_000;

        let mut db = prepare_jovian_db(DA_FOOTPRINT_GAS_SCALAR);
        let base_chain_upgrades = ChainUpgrades::new(
            BaseUpgrade::mainnet()
                .into_iter()
                .chain(vec![(BaseUpgrade::Jovian, ForkCondition::Timestamp(JOVIAN_TIMESTAMP))]),
        );

        let mut executor =
            build_executor(&mut db, &base_chain_upgrades, GAS_LIMIT, JOVIAN_TIMESTAMP);

        let tx_inner = TxLegacy { gas_limit: GAS_LIMIT, ..Default::default() };

        let tx = Recovered::new_unchecked(
            BaseTxEnvelope::Legacy(tx_inner.into_signed(Signature::new(
                Default::default(),
                Default::default(),
                Default::default(),
            ))),
            Address::ZERO,
        );
        let tx_env = tx.to_tx_env();

        assert!(executor.da_footprint_used == 0);

        let expected_da_footprint = executor.jovian_da_footprint_estimation(&tx_env, &tx).unwrap();

        // make sure we can use both `WithEncoded` and transaction itself as inputs.
        let gas_used_tx = executor.execute_transaction(&tx).expect("failed to execute transaction");

        // The gas used when executing the transaction should be the legacy value...
        assert!(gas_used_tx.tx_gas_used() < expected_da_footprint);

        // The gas used when finishing the executor should be the DA footprint since this is higher
        // than the legacy gas used and jovian is active...
        let (_, result) = executor.finish().expect("failed to finish executor");
        assert_eq!(result.blob_gas_used, expected_da_footprint);
        assert_eq!(result.gas_used, gas_used_tx.tx_gas_used());
        assert!(result.blob_gas_used > result.gas_used);
    }

    /// Builds a signed EIP-8130 transaction with the given `gas_limit`. When
    /// `with_payer` is set, a distinct payer is named with a resolvable k1 payer
    /// auth blob (`K1_AUTHENTICATOR || sig`), so `payer_auth` is non-zero.
    fn signed_eip8130(gas_limit: u64, with_payer: bool) -> Eip8130Signed {
        let payer_auth = if with_payer {
            let mut blob = vec![];
            blob.extend_from_slice(Eip8130Constants::K1_AUTHENTICATOR.as_slice());
            blob.extend_from_slice(&[0u8; 65]);
            Bytes::from(blob)
        } else {
            Bytes::new()
        };
        let tx = TxEip8130 {
            gas_limit,
            payer: with_payer.then(|| Address::with_last_byte(0x11)),
            ..Default::default()
        };
        Eip8130Signed::new(tx, Bytes::new(), payer_auth)
    }

    /// The block gas reservation for an EIP-8130 transaction must include
    /// `payer_auth` on top of the declared `gas_limit`: a transaction that fits
    /// on `gas_limit` alone is still rejected when `gas_limit + payer_auth`
    /// exceeds the available block gas.
    #[test]
    fn eip8130_block_gas_reservation_includes_payer_auth() {
        const GAS_LIMIT: u64 = 100_000;
        const JOVIAN_TIMESTAMP: u64 = 1746806402;

        let signed = signed_eip8130(GAS_LIMIT, true);
        let payer_auth = IntrinsicGas::max_payer_auth_cost(&signed).expect("payer auth cost");
        assert!(payer_auth > 0, "payer auth must be metered on top of gas_limit");

        // Block admits `gas_limit` alone but not `gas_limit + payer_auth`.
        let block_gas_limit = GAS_LIMIT + payer_auth - 1;

        let mut db = prepare_jovian_db(0);
        let base_chain_upgrades = ChainUpgrades::new(
            BaseUpgrade::mainnet()
                .into_iter()
                .chain(vec![(BaseUpgrade::Jovian, ForkCondition::Timestamp(JOVIAN_TIMESTAMP))]),
        );
        let mut executor =
            build_executor(&mut db, &base_chain_upgrades, block_gas_limit, JOVIAN_TIMESTAMP);

        let tx = Recovered::new_unchecked(
            BaseTxEnvelope::Eip8130(signed),
            Address::with_last_byte(0x22),
        );
        let err = executor.execute_transaction(&tx).expect_err("reservation must reject");
        match err {
            BlockExecutionError::Validation(
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit,
                    block_available_gas,
                },
            ) => {
                assert_eq!(transaction_gas_limit, GAS_LIMIT + payer_auth);
                assert_eq!(block_available_gas, block_gas_limit);
            }
            other => panic!("expected TransactionGasLimitMoreThanAvailableBlockGas, got {other:?}"),
        }
    }

    /// A self-paying EIP-8130 transaction meters no `payer_auth`, so its
    /// reservation is exactly `gas_limit`: it must not be rejected by the block
    /// gas pre-check when the block admits its declared `gas_limit`.
    #[test]
    fn self_pay_eip8130_block_gas_reservation_is_gas_limit() {
        const GAS_LIMIT: u64 = 100_000;
        const JOVIAN_TIMESTAMP: u64 = 1746806402;

        let signed = signed_eip8130(GAS_LIMIT, false);
        assert_eq!(
            IntrinsicGas::max_payer_auth_cost(&signed).expect("payer auth cost"),
            0,
            "self-pay meters no payer authentication",
        );

        let mut db = prepare_jovian_db(0);
        let base_chain_upgrades = ChainUpgrades::new(
            BaseUpgrade::mainnet()
                .into_iter()
                .chain(vec![(BaseUpgrade::Jovian, ForkCondition::Timestamp(JOVIAN_TIMESTAMP))]),
        );
        let mut executor =
            build_executor(&mut db, &base_chain_upgrades, GAS_LIMIT, JOVIAN_TIMESTAMP);

        let tx = Recovered::new_unchecked(
            BaseTxEnvelope::Eip8130(signed),
            Address::with_last_byte(0x22),
        );
        // Execution itself may fail for this synthetic transaction, but the block
        // gas pre-check must not: reserving exactly `gas_limit` fits the block.
        if let Err(BlockExecutionError::Validation(
            BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. },
        )) = executor.execute_transaction(&tx)
        {
            panic!("self-pay transaction must not be rejected by the block gas pre-check");
        }
    }
    #[test]
    #[cfg(feature = "std")]
    fn committed_receipts_preserve_phase_statuses_deposit_fields_and_cumulative_gas() {
        let factory = BaseBlockExecutorFactory::new(
            base_common_chain_config::ChainConfig::mainnet().clone(),
            BaseEvmFactory::default(),
        );
        let mut db =
            base_execution_evm_runtime::State::builder().with_database(EmptyDB::default()).build();
        let evm = factory.evm_factory.create_evm(
            &mut db,
            EvmEnv {
                block_env: BlockEnv { timestamp: U256::from(u64::MAX), ..Default::default() },
                ..Default::default()
            },
        );
        let mut executor = factory.create_executor(evm, BaseBlockExecutionCtx::default());
        crate::Eip8130PhaseStatuses::set(vec![1, 0]);
        for tx_type in [OpTxType::Eip8130, OpTxType::Deposit] {
            executor.commit_transaction(BaseTxResult {
                result: ResultAndState {
                    result: base_execution_evm_runtime::ExecutionResult::Success {
                        reason: base_execution_evm_runtime::SuccessReason::Return,
                        gas: base_execution_evm_runtime::ResultGas::new_with_state_gas(
                            21_000, 0, 0, 0,
                        ),
                        logs: vec![alloy_primitives::Log::default()],
                        output: base_execution_evm_runtime::Output::Call(Bytes::new()),
                    },
                    state: Default::default(),
                },
                blob_gas_used: 0,
                tx_type,
                is_deposit: tx_type == OpTxType::Deposit,
                sender: Address::ZERO,
                depositor: Some(AccountInfo { nonce: 42, ..Default::default() }),
            });
        }
        let BaseReceipt::Eip8130(receipt) = &executor.receipts()[0] else {
            panic!("expected EIP-8130 receipt")
        };
        assert_eq!(receipt.phase_statuses, [1, 0]);
        assert_eq!(receipt.inner.cumulative_gas_used, 21_000);
        assert_eq!(receipt.inner.logs.len(), 1);
        assert!(crate::Eip8130PhaseStatuses::take().is_empty());
        let deposit = executor.receipts()[1].as_deposit_receipt().unwrap();
        assert_eq!(deposit.deposit_nonce, Some(42));
        assert_eq!(deposit.deposit_receipt_version, Some(1));
        assert_eq!(deposit.inner.cumulative_gas_used, 42_000);
        assert_eq!(deposit.inner.logs.len(), 1);
    }
}
