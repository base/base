use std::{sync::Arc, time::Instant};

use alloy_consensus::{
    Block, Header, TxReceipt,
    transaction::{Recovered, TransactionMeta},
};
use alloy_eips::Encodable2718;
use alloy_evm::{
    Database as AlloyDatabase,
    block::{StateDB, SystemCaller},
};
use alloy_primitives::B256;
use alloy_rpc_types::TransactionTrait;
use alloy_rpc_types_eth::state::StateOverride;
use base_alloy_chains::BaseUpgrades;
use base_alloy_consensus::{OpPrimitives, OpReceipt, OpTxEnvelope};
use base_alloy_evm::ensure_create2_deployer;
use base_alloy_flz::tx_estimated_size_fjord as estimate_tx_compressed_size;
use base_common_rpc_types::{OpTransactionReceipt, Transaction};
use base_execution_rpc::OpReceiptBuilder as OpRpcReceiptBuilder;
use base_revm::{L1_BLOCK_CONTRACT, L1BlockInfo, OpHaltReason};
use reth_evm::{Evm, FromRecoveredTx};
use reth_rpc_convert::transaction::ConvertReceiptInput;
use revm::{
    Database, DatabaseCommit,
    context::{
        Block as _,
        result::{ExecutionResult, ResultAndState},
    },
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
    /// Per-transaction EVM execution time, if known.
    pub execution_time_us: Option<u128>,
}

#[derive(Debug)]
struct CachedTransactionExecution {
    receipt: OpTransactionReceipt,
    state: EvmState,
    result: ExecutionResult<OpHaltReason>,
    execution_time_us: Option<u128>,
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
    chain_spec: ChainSpec,

    prev_pending_blocks: Option<Arc<PendingBlocks>>,
    state_overrides: StateOverride,
}

impl<E, ChainSpec, DB> PendingStateBuilder<E, ChainSpec>
where
    E: Evm<DB = DB, HaltReason = OpHaltReason>,
    DB: Database + DatabaseCommit,
    E::Tx: FromRecoveredTx<OpTxEnvelope>,
    ChainSpec: BaseUpgrades + Clone,
{
    /// Creates a new pending state builder.
    pub fn new(
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
            chain_spec: chain_spec.clone(),
            receipt_builder: UnifiedReceiptBuilder::new(chain_spec),
        }
    }

    /// Consumes the builder and returns the database and state overrides.
    pub fn into_db_and_state_overrides(self) -> (DB, StateOverride) {
        (self.evm.into_db(), self.state_overrides)
    }

    /// Returns a mutable reference to the underlying database.
    pub fn db_mut(&mut self) -> &mut DB {
        self.evm.db_mut()
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

        // Check if we have all the data we need to reuse the previous execution.
        let cached_execution = self.prev_pending_blocks.as_ref().and_then(|p| {
            Some(CachedTransactionExecution {
                receipt: p.get_receipt(tx_hash)?.clone(),
                state: p.get_transaction_state(&tx_hash)?,
                result: p.get_transaction_result(&tx_hash)?.clone(),
                execution_time_us: p.get_execution_time(&tx_hash),
            })
        });

        // If cached, we can fill out pending block data using previous execution results
        // If not cached, we need to execute the transaction and build pending block data from scratch
        if let Some(cached_execution) = cached_execution {
            self.execute_with_cached_data(transaction, cached_execution, idx, effective_gas_price)
        } else {
            self.execute_with_evm(transaction, idx, effective_gas_price)
        }
    }

    /// Applies EIP-4788, EIP-2935, and Canyon create2 deployer pre-execution changes to the EVM.
    ///
    /// Must be called once per block, before executing any transactions. This mirrors the
    /// `apply_pre_execution_changes` behavior of [`base_alloy_evm::BaseBlockExecutor`] to ensure
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
        cached_execution: CachedTransactionExecution,
        idx: usize,
        effective_gas_price: u128,
    ) -> Result<ExecutedPendingTransaction, StateProcessorError> {
        let CachedTransactionExecution { receipt, state, result, execution_time_us } =
            cached_execution;

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

        Ok(ExecutedPendingTransaction {
            rpc_transaction,
            receipt,
            state,
            result,
            execution_time_us,
        })
    }

    fn jovian_da_footprint_estimation(
        &mut self,
        tx_env: &Recovered<OpTxEnvelope>,
    ) -> Result<u64, StateProcessorError> {
        // Try to use the enveloped tx if it exists, otherwise use the encoded 2718 bytes
        let encoded = estimate_tx_compressed_size(tx_env.into_encoded().encoded_bytes())
            .saturating_div(1_000_000);

        // Load the L1 block contract into the cache. If the L1 block contract is not pre-loaded the
        // database will panic when trying to fetch the DA footprint gas scalar.
        self.evm.db_mut().basic(L1_BLOCK_CONTRACT).map_err(|err| {
            StateProcessorError::Execution(ExecutionError::DaFootprintEstimation(err.to_string()))
        })?;

        let da_footprint_gas_scalar = L1BlockInfo::fetch_da_footprint_gas_scalar(self.evm.db_mut())
            .map_err(|err| {
                StateProcessorError::Execution(ExecutionError::DaFootprintEstimation(
                    err.to_string(),
                ))
            })?
            .into();

        Ok(encoded.saturating_mul(da_footprint_gas_scalar))
    }

    /// Executes the transaction through the EVM and builds the result from scratch.
    fn execute_with_evm(
        &mut self,
        transaction: Recovered<OpTxEnvelope>,
        idx: usize,
        effective_gas_price: u128,
    ) -> Result<ExecutedPendingTransaction, StateProcessorError> {
        let tx_hash = transaction.tx_hash();

        let is_deposit = transaction.is_deposit();

        let da_footprint_used = if self
            .chain_spec
            .is_jovian_active_at_timestamp(self.evm.block().timestamp().saturating_to())
            && !is_deposit
        {
            self.jovian_da_footprint_estimation(&transaction)?
        } else {
            0
        };

        let start = Instant::now();
        let transact_result = self.evm.transact(&transaction);
        let elapsed_us = start.elapsed().as_micros();

        match transact_result {
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

                let mut op_receipt = OpRpcReceiptBuilder::new(
                    self.receipt_builder.chain_spec(),
                    input,
                    &mut self.l1_block_info,
                )
                .map_err(|e| ExecutionError::RpcReceiptBuild(e.to_string()))?
                .build();

                op_receipt.inner.blob_gas_used = Some(da_footprint_used);
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
                    execution_time_us: Some(elapsed_us),
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

    use alloy_consensus::{Block, BlockBody, Header, Signed};
    use alloy_eips::eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE};
    use alloy_primitives::{Address, B256, TxKind, U256, address, uint};
    use alloy_rpc_types_engine::PayloadId;
    use base_alloy_consensus::OpTxEnvelope;
    use base_alloy_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
    };
    use base_execution_chainspec::OpChainSpecBuilder;
    use base_execution_evm::OpEvmConfig;
    use base_revm::L1BlockInfo;
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

    const L1_BLOCK_ADDRESS: Address =
        Address::new([0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x15]);

    const DA_FOOTPRINT_GAS_SCALAR_SLOT: U256 = uint!(8_U256);

    fn create_legacy_tx() -> alloy_consensus::transaction::Recovered<OpTxEnvelope> {
        let tx = alloy_consensus::TxLegacy {
            chain_id: Some(8453),
            nonce: 0,
            gas_price: 1_000_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Default::default(),
        };

        let envelope = OpTxEnvelope::Legacy(Signed::new_unchecked(
            tx,
            alloy_primitives::Signature::test_signature(),
            B256::ZERO,
        ));

        alloy_consensus::transaction::Recovered::new_unchecked(envelope, Address::ZERO)
    }

    #[test]
    fn cached_execute_transaction_preserves_execution_time_from_prev_pending_blocks() {
        let chain_spec = Arc::new(OpChainSpecBuilder::base_mainnet().build());
        let evm_config = OpEvmConfig::optimism(Arc::clone(&chain_spec));

        let header = Header {
            number: 1,
            timestamp: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            Address::ZERO,
            AccountInfo {
                balance: U256::from(1_000_000_000_000_000_000u128),
                ..Default::default()
            },
        );

        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);
        let pending_block = Block { header: header.clone(), body: Default::default() };
        let mut first_builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            evm,
            pending_block,
            None,
            L1BlockInfo::default(),
            StateOverride::default(),
        );

        let tx = create_legacy_tx();
        let tx_hash = tx.tx_hash();
        let first_result =
            first_builder.execute_transaction(0, tx).expect("transaction execution failed");

        let mut pending_blocks_builder = crate::PendingBlocksBuilder::new();
        pending_blocks_builder
            .with_header(alloy_consensus::Sealed::new_unchecked(header.clone(), B256::ZERO));
        pending_blocks_builder.with_flashblocks([Flashblock {
            payload_id: PayloadId::default(),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::ZERO,
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number: header.number,
                gas_limit: header.gas_limit,
                timestamp: header.timestamp,
                extra_data: Default::default(),
                base_fee_per_gas: U256::from(header.base_fee_per_gas.unwrap_or_default()),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Default::default(),
                gas_used: first_result.receipt.inner.gas_used,
                block_hash: B256::ZERO,
                transactions: vec![],
                withdrawals: vec![],
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: Metadata { block_number: header.number },
        }]);
        pending_blocks_builder.with_receipt(tx_hash, first_result.receipt.clone());
        pending_blocks_builder.with_transaction_state(tx_hash, first_result.state.clone());
        pending_blocks_builder.with_transaction_result(tx_hash, first_result.result);
        pending_blocks_builder.with_execution_time(tx_hash, 1_234);

        let prev_pending_blocks =
            Arc::new(pending_blocks_builder.build().expect("should build cached pending blocks"));

        let second_evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let second_evm = evm_config.evm_with_env(InMemoryDB::default(), second_evm_env);
        let second_pending_block = Block { header, body: Default::default() };
        let mut second_builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            second_evm,
            second_pending_block,
            Some(prev_pending_blocks),
            L1BlockInfo::default(),
            StateOverride::default(),
        );

        let cached_result = second_builder
            .execute_transaction(0, create_legacy_tx())
            .expect("cached transaction execution failed");

        assert_eq!(cached_result.execution_time_us, Some(1_234));
    }

    #[test]
    fn flashblock_tx_has_nonzero_blob_gas_used_when_jovian_active() {
        let chain_spec = Arc::new(OpChainSpecBuilder::base_mainnet().jovian_activated().build());
        let mut db = InMemoryDB::default();

        let sender_info = AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000u128),
            ..Default::default()
        };
        db.insert_account_info(Address::ZERO, sender_info);

        // Seed L1 block contract slot 8 with DA footprint gas scalar at bytes [18..20] (big-endian u16).
        let da_scalar: u16 = 100;
        let mut slot_value = [0u8; 32];
        slot_value[18..20].copy_from_slice(&da_scalar.to_be_bytes());
        db.insert_account_info(L1_BLOCK_ADDRESS, revm::state::AccountInfo::default());
        db.insert_account_storage(
            L1_BLOCK_ADDRESS,
            DA_FOOTPRINT_GAS_SCALAR_SLOT,
            U256::from_be_bytes(slot_value),
        )
        .expect("failed to insert L1 block storage");

        let header = Header {
            timestamp: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };
        let evm_config = OpEvmConfig::optimism(Arc::clone(&chain_spec));
        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);

        let pending_block = Block { header, body: Default::default() };

        let mut builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            evm,
            pending_block,
            None,
            L1BlockInfo::default(),
            StateOverride::default(),
        );

        let tx = create_legacy_tx();
        let result = builder.execute_transaction(0, tx).expect("transaction execution failed");

        let blob_gas_used =
            result.receipt.inner.blob_gas_used.expect("blob_gas_used should be set");
        assert!(
            blob_gas_used > 0,
            "blob_gas_used should be > 0 when Jovian is active for non-deposit tx, got {blob_gas_used}"
        );
    }

    #[test]
    fn flashblock_tx_has_zero_blob_gas_used_when_jovian_inactive() {
        let chain_spec = Arc::new(OpChainSpecBuilder::base_mainnet().build());
        let mut db = InMemoryDB::default();

        let sender_info = AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000u128),
            ..Default::default()
        };
        db.insert_account_info(Address::ZERO, sender_info);

        let header = Header {
            timestamp: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };
        let evm_config = OpEvmConfig::optimism(Arc::clone(&chain_spec));
        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);

        let pending_block = Block { header, body: Default::default() };

        let mut builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            evm,
            pending_block,
            None,
            L1BlockInfo::default(),
            StateOverride::default(),
        );

        let tx = create_legacy_tx();
        let result = builder.execute_transaction(0, tx).expect("transaction execution failed");

        let blob_gas_used =
            result.receipt.inner.blob_gas_used.expect("blob_gas_used should be set");
        assert_eq!(
            blob_gas_used, 0,
            "blob_gas_used should be 0 when Jovian is inactive, got {blob_gas_used}"
        );
    }

    #[test]
    fn flashblock_deposit_tx_has_zero_blob_gas_used_when_jovian_active() {
        let chain_spec = Arc::new(OpChainSpecBuilder::base_mainnet().jovian_activated().build());
        let mut db = InMemoryDB::default();

        let deposit_sender: Address = address!("0x1234567890123456789012345678901234567890");
        let sender_info = AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000u128),
            ..Default::default()
        };
        db.insert_account_info(deposit_sender, sender_info);

        // Seed L1 block contract slot 8 with DA footprint gas scalar at bytes [18..20] (big-endian u16).
        let da_scalar: u16 = 100;
        let mut slot_value = [0u8; 32];
        slot_value[18..20].copy_from_slice(&da_scalar.to_be_bytes());
        db.insert_account_info(L1_BLOCK_ADDRESS, AccountInfo::default());
        db.insert_account_storage(
            L1_BLOCK_ADDRESS,
            DA_FOOTPRINT_GAS_SCALAR_SLOT,
            U256::from_be_bytes(slot_value),
        )
        .expect("failed to insert L1 block storage");

        let header = Header {
            timestamp: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };
        let evm_config = OpEvmConfig::optimism(Arc::clone(&chain_spec));
        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);

        let pending_block = Block { header, body: Default::default() };

        let mut builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            evm,
            pending_block,
            None,
            L1BlockInfo::default(),
            StateOverride::default(),
        );

        let deposit_tx = base_alloy_consensus::TxDeposit {
            source_hash: B256::ZERO,
            from: deposit_sender,
            to: TxKind::Call(Address::ZERO),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 21_000,
            is_system_transaction: false,
            input: Default::default(),
        };

        let sealed = alloy_consensus::Sealed::new_unchecked(deposit_tx, B256::ZERO);
        let envelope = OpTxEnvelope::Deposit(sealed);
        let tx = alloy_consensus::transaction::Recovered::new_unchecked(envelope, deposit_sender);

        let result = builder.execute_transaction(0, tx).expect("deposit execution failed");

        let blob_gas_used =
            result.receipt.inner.blob_gas_used.expect("blob_gas_used should be set");
        assert_eq!(
            blob_gas_used, 0,
            "blob_gas_used should be 0 for deposit tx even when Jovian is active"
        );
    }
}
