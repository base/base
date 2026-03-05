use std::sync::Arc;

use alloy_consensus::{
    Block, Header, TxReceipt,
    transaction::{Recovered, TransactionMeta},
};
use alloy_evm::{
    Database as AlloyDatabase,
    block::{StateDB, SystemCaller},
};
use alloy_primitives::B256;
use alloy_rpc_types::TransactionTrait;
use alloy_rpc_types_eth::state::StateOverride;
use base_alloy_consensus::{OpReceipt, OpTxEnvelope};
use base_alloy_evm::ensure_create2_deployer;
use base_alloy_rpc_types::{OpTransactionReceipt, Transaction};
use base_execution_forks::OpHardforks;
use base_execution_primitives::OpPrimitives;
use base_execution_rpc::OpReceiptBuilder as OpRpcReceiptBuilder;
use base_revm::{L1BlockInfo, OpHaltReason};
use reth_evm::{Evm, FromRecoveredTx};
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

    /// Executes a single transaction and updates internal state.
    /// Should be called in order for each transaction.
    #[instrument(level = "debug", skip_all, fields(tx_hash = %transaction.tx_hash(), idx = idx))]
    pub fn execute_transaction(
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

    /// Applies EIP-4788, EIP-2935, and Canyon create2 deployer pre-execution changes to the EVM.
    ///
    /// Must be called once per block, before executing any transactions. This mirrors the
    /// `apply_pre_execution_changes` behavior of [`base_alloy_evm::OpBlockExecutor`] to ensure
    /// that the cached execution results match what the validator computes.
    pub fn apply_pre_execution_changes(
        &mut self,
        parent_hash: B256,
        parent_beacon_block_root: Option<B256>,
    ) -> Result<(), StateProcessorError>
    where
        DB: AlloyDatabase + StateDB,
        ChainSpec: Clone,
    {
        let spec = self.receipt_builder.chain_spec();
        let state_clear_flag = spec.is_spurious_dragon_active_at_block(self.pending_block.number);
        self.evm.db_mut().set_state_clear_flag(state_clear_flag);

        let mut system_caller = SystemCaller::new(spec.clone());
        system_caller
            .apply_blockhashes_contract_call(parent_hash, &mut self.evm)
            .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
        system_caller
            .apply_beacon_root_contract_call(parent_beacon_block_root, &mut self.evm)
            .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;

        ensure_create2_deployer(spec, self.pending_block.timestamp, self.evm.db_mut())
            .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;

        Ok(())
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{Block, BlockBody, Header};
    use alloy_eips::eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE};
    use alloy_primitives::{B256, U256};
    use base_execution_chainspec::OpChainSpecBuilder;
    use base_execution_evm::OpEvmConfig;
    use reth_evm::ConfigureEvm;
    use reth_revm::State;
    use revm::{
        database::InMemoryDB,
        state::{AccountInfo, Bytecode},
    };

    use super::*;

    // Base mainnet Ecotone activation timestamp, after which EIP-4788 is active.
    const BASE_MAINNET_ECOTONE_TIMESTAMP: u64 = 1_710_374_401;
    // A timestamp just after Ecotone activation.
    const POST_ECOTONE_TIMESTAMP: u64 = BASE_MAINNET_ECOTONE_TIMESTAMP + 1;
    // EIP-4788 ring buffer length (hardcoded in the contract bytecode).
    const BEACON_ROOTS_HISTORY_BUFFER_LENGTH: u64 = 8191;

    fn make_db_with_beacon_roots_contract() -> State<InMemoryDB> {
        let mut db = State::builder().with_database(InMemoryDB::default()).build();
        let code = Bytecode::new_raw(BEACON_ROOTS_CODE.clone());
        let code_hash = code.hash_slow();
        db.insert_account(
            BEACON_ROOTS_ADDRESS,
            AccountInfo { code: Some(code), code_hash, nonce: 1, ..Default::default() },
        );
        db
    }

    #[test]
    fn apply_pre_execution_changes_stores_beacon_root_in_eip4788_contract() {
        let db = make_db_with_beacon_roots_contract();

        let chain_spec = Arc::new(OpChainSpecBuilder::base_mainnet().build());
        let evm_config = OpEvmConfig::optimism(Arc::clone(&chain_spec));
        let header = Header { timestamp: POST_ECOTONE_TIMESTAMP, number: 1, ..Default::default() };
        let evm_env = evm_config.evm_env(&header).expect("failed to build evm env");
        let evm = evm_config.evm_with_env(db, evm_env);
        let pending_block = Block {
            header: Header { timestamp: POST_ECOTONE_TIMESTAMP, number: 1, ..Default::default() },
            body: BlockBody::<OpTxEnvelope>::default(),
        };
        let mut builder = PendingStateBuilder::new(
            chain_spec,
            evm,
            pending_block,
            None,
            L1BlockInfo::default(),
            Default::default(),
        );

        let parent_beacon_block_root = B256::from([0xab; 32]);
        builder
            .apply_pre_execution_changes(B256::ZERO, Some(parent_beacon_block_root))
            .expect("apply_pre_execution_changes should succeed");

        let (db, _) = builder.into_db_and_state_overrides();

        // EIP-4788 stores parent_beacon_block_root at:
        //   slot = timestamp % HISTORY_BUFFER_LENGTH + HISTORY_BUFFER_LENGTH
        let timestamp_idx = POST_ECOTONE_TIMESTAMP % BEACON_ROOTS_HISTORY_BUFFER_LENGTH;
        let root_slot = U256::from(timestamp_idx + BEACON_ROOTS_HISTORY_BUFFER_LENGTH);
        let beacon_account = db
            .cache
            .accounts
            .get(&BEACON_ROOTS_ADDRESS)
            .expect("beacon roots contract should be in cache after commit");
        let storage = &beacon_account
            .account
            .as_ref()
            .expect("beacon roots account should be populated")
            .storage;
        let stored_root = *storage.get(&root_slot).expect("beacon root slot should be written");

        assert_eq!(
            stored_root,
            U256::from_be_bytes(parent_beacon_block_root.0),
            "EIP-4788 should store parent_beacon_block_root at timestamp-indexed slot"
        );
    }

    #[test]
    fn apply_pre_execution_changes_pre_ecotone_with_no_beacon_root_is_noop_for_eip4788() {
        let db = make_db_with_beacon_roots_contract();

        // Use a timestamp before Ecotone activation so EIP-4788 is inactive.
        // In this regime None is valid (no beacon root contract call is made).
        let pre_ecotone_timestamp = BASE_MAINNET_ECOTONE_TIMESTAMP - 1;

        let chain_spec = Arc::new(OpChainSpecBuilder::base_mainnet().build());
        let evm_config = OpEvmConfig::optimism(Arc::clone(&chain_spec));
        let header = Header { timestamp: pre_ecotone_timestamp, number: 1, ..Default::default() };
        let evm_env = evm_config.evm_env(&header).expect("failed to build evm env");
        let evm = evm_config.evm_with_env(db, evm_env);
        let pending_block = Block {
            header: Header { timestamp: pre_ecotone_timestamp, number: 1, ..Default::default() },
            body: BlockBody::<OpTxEnvelope>::default(),
        };
        let mut builder = PendingStateBuilder::new(
            chain_spec,
            evm,
            pending_block,
            None,
            L1BlockInfo::default(),
            Default::default(),
        );

        builder
            .apply_pre_execution_changes(B256::ZERO, None)
            .expect("apply_pre_execution_changes should succeed pre-Ecotone with no beacon root");

        let (db, _) = builder.into_db_and_state_overrides();

        // EIP-4788 is inactive pre-Ecotone, so the contract should have no storage writes.
        let beacon_account = db.cache.accounts.get(&BEACON_ROOTS_ADDRESS);
        let has_storage_writes =
            beacon_account.and_then(|a| a.account.as_ref()).is_some_and(|a| !a.storage.is_empty());
        assert!(
            !has_storage_writes,
            "EIP-4788 contract should not be called and have no storage writes pre-Ecotone"
        );
    }
}
