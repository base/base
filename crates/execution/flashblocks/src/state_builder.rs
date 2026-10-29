use std::{sync::Arc, time::Instant};

use alloy_consensus::{
    Block, Header, TxReceipt,
    transaction::{Recovered, TransactionMeta},
};
use alloy_primitives::{B256, Bytes, U256};
use alloy_rpc_types::TransactionTrait;
use alloy_rpc_types_eth::state::StateOverride;
use base_common_chains::Upgrades;
use base_common_consensus::{BasePrimitives, BaseReceipt, BaseTxEnvelope};
use base_common_evm2::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BaseBlockState, BaseEvmTypes,
    BaseTxEnvelope as BaseTxEnv, L1Block,
};
use base_common_l1_fees::L1FeeParams;
use base_common_rpc_types::{BaseTransactionReceipt, Transaction};
use base_execution_evm::{BaseRethBlockState, UpgradeActivations};
use base_execution_rpc::BaseReceiptBuilder as BaseRpcReceiptBuilder;
use evm2::{
    Evm, TxResult, TxResultWithState,
    evm::{AccountChanges, Cache, PendingState},
};
use reth_evm::{BundleState, Evm as _, FromRecoveredTx};
use reth_rpc_convert::transaction::ConvertReceiptInput;

use crate::{ExecutionError, PendingBlocks, StateProcessorError, UnifiedReceiptBuilder};

/// Represents the result of executing or fetching a cached pending transaction.
#[derive(Debug, Clone)]
pub struct ExecutedPendingTransaction {
    /// The RPC transaction.
    pub rpc_transaction: Transaction,
    /// The receipt of the transaction.
    pub receipt: BaseTransactionReceipt,
    /// The transaction's state changes.
    pub state: PendingState,
    /// The execution result of the transaction.
    pub result: TxResult<BaseEvmTypes>,
    /// Per-transaction EVM execution time, if known.
    pub execution_time_us: Option<u128>,
}

#[derive(Debug)]
struct CachedTransactionExecution {
    receipt: BaseTransactionReceipt,
    state: PendingState,
    result: TxResult<BaseEvmTypes>,
    execution_time_us: Option<u128>,
}

/// What a [`PendingStateBuilder`] leaves behind: the state it read and wrote, the block's state
/// changes, and the pending RPC state overrides.
#[derive(Debug)]
pub struct PendingStateOutcome {
    /// Accounts, storage, and code the builder read or wrote, for the next build on this state.
    pub cache: Cache,
    /// The state changes of the transactions (and pre-execution changes) the builder applied.
    pub bundle_state: BundleState,
    /// The pending RPC state overrides, extended with the executed transactions' state.
    pub state_overrides: StateOverride,
}

/// Executes or fetches cached values for transactions in a flashblock.
#[derive(Debug)]
pub struct PendingStateBuilder<'a, ChainSpec> {
    cumulative_gas_used: u64,
    next_log_index: usize,

    evm: Evm<'a, BaseEvmTypes>,
    block_state: BaseRethBlockState,
    pending_block: Block<BaseTxEnvelope, Header>,
    l1_fee_params: L1FeeParams,
    receipt_builder: UnifiedReceiptBuilder<ChainSpec>,
    chain_spec: ChainSpec,

    prev_pending_blocks: Option<Arc<PendingBlocks>>,
    state_overrides: StateOverride,
}

impl<'a, ChainSpec> PendingStateBuilder<'a, ChainSpec>
where
    ChainSpec: Upgrades + Clone,
{
    /// Creates a new pending state builder.
    pub fn new(
        chain_spec: ChainSpec,
        evm: Evm<'a, BaseEvmTypes>,
        pending_block: Block<BaseTxEnvelope, Header>,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        l1_fee_params: L1FeeParams,
        state_overrides: StateOverride,
    ) -> Self {
        Self {
            pending_block,
            evm,
            block_state: BaseRethBlockState::default(),
            cumulative_gas_used: 0,
            next_log_index: 0,
            prev_pending_blocks,
            l1_fee_params,
            state_overrides,
            chain_spec: chain_spec.clone(),
            receipt_builder: UnifiedReceiptBuilder::new(chain_spec),
        }
    }

    /// Consumes the builder and returns what it read, wrote, and overrode.
    pub fn finish(mut self) -> PendingStateOutcome {
        PendingStateOutcome {
            cache: core::mem::take(&mut self.evm.overlay_db_mut().cache),
            bundle_state: self.block_state.state.into_bundle(),
            state_overrides: self.state_overrides,
        }
    }

    /// Seeds block-level offsets when appending transactions to an already-executed pending block.
    pub const fn set_execution_offsets(&mut self, cumulative_gas_used: u64, next_log_index: usize) {
        self.cumulative_gas_used = cumulative_gas_used;
        self.next_log_index = next_log_index;
    }

    /// Returns the cumulative gas used for the current pending block.
    pub const fn cumulative_gas_used(&self) -> u64 {
        self.cumulative_gas_used
    }

    /// Returns the next log index for the current pending block.
    pub const fn next_log_index(&self) -> usize {
        self.next_log_index
    }

    /// Executes a single transaction and updates internal state.
    /// Should be called in order for each transaction.
    #[instrument(level = "debug", skip_all, fields(tx_hash = %transaction.tx_hash(), idx = idx))]
    pub fn execute_transaction(
        &mut self,
        idx: usize,
        transaction: Recovered<BaseTxEnvelope>,
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

    /// Applies the block's pre-execution changes: the EIP-2935 and EIP-4788 system calls and the
    /// Base upgrade transition changes.
    ///
    /// Must be called once per block, before executing any transactions. It runs the same
    /// pre-execution steps as [`BaseBlockExecutor`], so the cached execution results match what
    /// the validator computes.
    pub fn apply_pre_execution_changes(
        self,
        ctx: BaseBlockExecutionCtx,
    ) -> Result<Self, StateProcessorError> {
        let mut executor = BaseBlockExecutor::with_block_state(self.evm, ctx, self.block_state);
        executor
            .apply_pre_execution()
            .and_then(|()| executor.apply_transition_hooks(&UpgradeActivations(&self.chain_spec)))
            .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
        let (evm, _, block_state) = executor.finish();
        Ok(Self { evm, block_state, ..self })
    }

    /// Builds transaction result from cached receipt and state data.
    fn execute_with_cached_data(
        &mut self,
        transaction: Recovered<BaseTxEnvelope>,
        cached_execution: CachedTransactionExecution,
        idx: usize,
        effective_gas_price: u128,
    ) -> Result<ExecutedPendingTransaction, StateProcessorError> {
        let CachedTransactionExecution { receipt, state, result, execution_time_us } =
            cached_execution;

        let (deposit_receipt_version, deposit_nonce) = if transaction.is_deposit() {
            let BaseReceipt::Deposit(deposit_receipt) = &receipt.inner.inner.receipt else {
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
                block_timestamp: Some(self.pending_block.timestamp),
                transaction_index: Some(idx as u64),
                effective_gas_price: Some(effective_gas_price),
            },
            deposit_nonce,
            deposit_receipt_version,
            block_timestamp_ms: None,
        };

        self.cumulative_gas_used = self
            .cumulative_gas_used
            .checked_add(receipt.inner.gas_used)
            .ok_or(ExecutionError::GasOverflow)?;
        self.next_log_index += receipt.inner.logs().len();

        self.block_state.commit(&mut self.evm, state.clone());

        Ok(ExecutedPendingTransaction {
            rpc_transaction,
            receipt,
            state,
            result,
            execution_time_us,
        })
    }

    /// Returns the Jovian DA footprint of `tx_env`, priced as block execution prices it.
    fn jovian_da_footprint_estimation(
        &mut self,
        tx_env: &Recovered<BaseTxEnv>,
    ) -> Result<u64, StateProcessorError> {
        L1Block::ensure_fee_params(&mut self.evm)
            .map_err(|err| ExecutionError::DaFootprintEstimation(err.to_string()))?;
        let enveloped = tx_env.enveloped().map(|bytes| bytes.as_ref()).unwrap_or_default();
        Ok(self.evm.block().ext.jovian_da_footprint(enveloped))
    }

    /// Adds an executed transaction's account changes to the pending RPC state overrides.
    fn override_state(&mut self, account: AccountChanges<'_>) {
        let info = account.current;
        // The override holds the unpadded original bytecode so `EXTCODESIZE` stays 0 for
        // code-less EOAs. Execution does not always load an account's code, so unloaded code is
        // read from the state the transaction executed on.
        let code = match info {
            None => Some(Bytes::new()),
            Some(info) if info.is_empty_code_hash() => Some(Bytes::new()),
            Some(info) => match &info.code {
                Some(code) => Some(code.original_bytes()),
                None => self
                    .evm
                    .state_db_mut()
                    .get_code_by_hash(&info.code_hash)
                    .ok()
                    .map(|code| code.original_bytes()),
            },
        };

        let existing_override = self.state_overrides.entry(account.address).or_default();
        existing_override.balance = Some(info.map_or(U256::ZERO, |info| info.balance));
        existing_override.nonce = Some(info.map_or(0, |info| info.nonce));
        existing_override.code = code;

        let existing = existing_override.state_diff.get_or_insert_with(Default::default);
        existing.extend(
            account
                .storage
                .slots
                .iter()
                .map(|(&key, slot)| (B256::from(key), B256::from(slot.value.current))),
        );
    }

    /// Executes the transaction through the EVM and builds the result from scratch.
    fn execute_with_evm(
        &mut self,
        transaction: Recovered<BaseTxEnvelope>,
        idx: usize,
        effective_gas_price: u128,
    ) -> Result<ExecutedPendingTransaction, StateProcessorError> {
        let tx_hash = transaction.tx_hash();

        let is_deposit = transaction.is_deposit();
        let tx_env = Recovered::new_unchecked(
            BaseTxEnv::from_recovered_tx(transaction.clone()),
            transaction.signer(),
        );

        let da_footprint_used = if self
            .chain_spec
            .is_jovian_active_at_timestamp(self.evm.block().timestamp.saturating_to())
            && !is_deposit
        {
            self.jovian_da_footprint_estimation(&tx_env)?
        } else {
            0
        };

        let start = Instant::now();
        let transact_result = self.evm.transact(&tx_env).map(|executed| executed.detach());
        let elapsed_us = start.elapsed().as_micros();

        let TxResultWithState { result, pending_state: state, .. } =
            transact_result.map_err(|e| ExecutionError::TransactionFailed {
                tx_hash,
                sender: transaction.signer(),
                reason: format!("{e:?}"),
            })?;

        let gas_used = result.tx_gas_used();
        for account in state.account_changes() {
            self.override_state(account);
        }

        self.cumulative_gas_used =
            self.cumulative_gas_used.checked_add(gas_used).ok_or(ExecutionError::GasOverflow)?;

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
        let input: ConvertReceiptInput<'_, BasePrimitives> = ConvertReceiptInput {
            receipt: receipt.clone(),
            tx: Recovered::new_unchecked(&transaction, sender),
            gas_used,
            next_log_index: self.next_log_index,
            meta,
        };

        let mut base_receipt = BaseRpcReceiptBuilder::new(
            self.receipt_builder.chain_spec(),
            input,
            &mut self.l1_fee_params,
        )
        .map_err(|e| ExecutionError::RpcReceiptBuild(e.to_string()))?
        .build();

        base_receipt.inner.blob_gas_used = Some(da_footprint_used);
        self.next_log_index += receipt.logs().len();

        let (deposit_receipt_version, deposit_nonce) = if transaction.is_deposit() {
            let BaseReceipt::Deposit(deposit_receipt) = &base_receipt.inner.inner.receipt else {
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
                block_timestamp: Some(self.pending_block.timestamp),
                transaction_index: Some(idx as u64),
                effective_gas_price: Some(effective_gas_price),
            },
            deposit_nonce,
            deposit_receipt_version,
            block_timestamp_ms: None,
        };
        self.block_state.commit(&mut self.evm, state.clone());

        Ok(ExecutedPendingTransaction {
            rpc_transaction,
            receipt: base_receipt,
            state,
            result,
            execution_time_us: Some(elapsed_us),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{Block, BlockBody, Header, Signed};
    use alloy_eips::eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE};
    use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, keccak256, uint};
    use alloy_rpc_types_engine::PayloadId;
    use base_common_consensus::BaseTxEnvelope;
    use base_common_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
    };
    use base_execution_chainspec::BaseChainSpecBuilder;
    use base_execution_evm::BaseEvmConfig;
    use evm2::{
        bytecode::Bytecode,
        evm::{AccountInfo, InMemoryDB},
    };
    use reth_evm::ConfigureEvm;

    use super::*;

    // Base mainnet Ecotone activation timestamp, after which EIP-4788 is active.
    const BASE_MAINNET_ECOTONE_TIMESTAMP: u64 = 1_710_374_401;
    // A timestamp just after Ecotone activation.
    const POST_ECOTONE_TIMESTAMP: u64 = BASE_MAINNET_ECOTONE_TIMESTAMP + 1;
    // EIP-4788 ring buffer length (hardcoded in the contract bytecode).
    const BEACON_ROOTS_HISTORY_BUFFER_LENGTH: u64 = 8191;

    fn make_db_with_beacon_roots_contract() -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            &BEACON_ROOTS_ADDRESS,
            AccountInfo::default()
                .with_nonce(1)
                .with_code(Bytecode::new_raw(BEACON_ROOTS_CODE.clone())),
        );
        db
    }

    fn execution_ctx(parent_beacon_block_root: Option<B256>) -> BaseBlockExecutionCtx {
        BaseBlockExecutionCtx {
            parent_hash: B256::ZERO,
            parent_beacon_block_root,
            extra_data: Bytes::new(),
        }
    }

    fn funded() -> AccountInfo {
        AccountInfo::default().with_balance(U256::from(1_000_000_000_000_000_000u128))
    }

    #[test]
    fn apply_pre_execution_changes_stores_beacon_root_in_eip4788_contract() {
        let db = make_db_with_beacon_roots_contract();

        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().build());
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let header = Header { timestamp: POST_ECOTONE_TIMESTAMP, number: 1, ..Default::default() };
        let evm_env = evm_config.evm_env(&header).expect("failed to build evm env");
        let evm = evm_config.evm_with_env(db, evm_env);
        let pending_block = Block {
            header: Header { timestamp: POST_ECOTONE_TIMESTAMP, number: 1, ..Default::default() },
            body: BlockBody::<BaseTxEnvelope>::default(),
        };
        let builder = PendingStateBuilder::new(
            chain_spec,
            evm,
            pending_block,
            None,
            L1FeeParams::default(),
            Default::default(),
        );

        let parent_beacon_block_root = B256::from([0xab; 32]);
        let outcome = builder
            .apply_pre_execution_changes(execution_ctx(Some(parent_beacon_block_root)))
            .expect("apply_pre_execution_changes should succeed")
            .finish();

        // EIP-4788 stores parent_beacon_block_root at:
        //   slot = timestamp % HISTORY_BUFFER_LENGTH + HISTORY_BUFFER_LENGTH
        let timestamp_idx = POST_ECOTONE_TIMESTAMP % BEACON_ROOTS_HISTORY_BUFFER_LENGTH;
        let root_slot = U256::from(timestamp_idx + BEACON_ROOTS_HISTORY_BUFFER_LENGTH);
        let storage = &outcome
            .cache
            .storage
            .get(&BEACON_ROOTS_ADDRESS)
            .expect("beacon roots contract storage should be in cache after commit")
            .slots;
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

        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().build());
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let header = Header { timestamp: pre_ecotone_timestamp, number: 1, ..Default::default() };
        let evm_env = evm_config.evm_env(&header).expect("failed to build evm env");
        let evm = evm_config.evm_with_env(db, evm_env);
        let pending_block = Block {
            header: Header { timestamp: pre_ecotone_timestamp, number: 1, ..Default::default() },
            body: BlockBody::<BaseTxEnvelope>::default(),
        };
        let builder = PendingStateBuilder::new(
            chain_spec,
            evm,
            pending_block,
            None,
            L1FeeParams::default(),
            Default::default(),
        );

        let outcome = builder
            .apply_pre_execution_changes(execution_ctx(None))
            .expect("apply_pre_execution_changes should succeed pre-Ecotone with no beacon root")
            .finish();

        // EIP-4788 is inactive pre-Ecotone, so the contract should have no storage writes.
        let has_storage_writes = outcome
            .cache
            .storage
            .get(&BEACON_ROOTS_ADDRESS)
            .is_some_and(|storage| !storage.slots.is_empty());
        assert!(
            !has_storage_writes,
            "EIP-4788 contract should not be called and have no storage writes pre-Ecotone"
        );
    }

    const L1_BLOCK_ADDRESS: Address =
        Address::new([0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x15]);

    const DA_FOOTPRINT_GAS_SCALAR_SLOT: U256 = uint!(8_U256);

    fn create_legacy_tx() -> alloy_consensus::transaction::Recovered<BaseTxEnvelope> {
        let tx = alloy_consensus::TxLegacy {
            chain_id: Some(8453),
            nonce: 0,
            gas_price: 1_000_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Default::default(),
        };

        let envelope = BaseTxEnvelope::Legacy(Signed::new_unchecked(
            tx,
            alloy_primitives::Signature::test_signature(),
            B256::ZERO,
        ));

        alloy_consensus::transaction::Recovered::new_unchecked(envelope, Address::ZERO)
    }

    #[test]
    fn cached_execute_transaction_preserves_timing_from_prev_pending_blocks() {
        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().build());
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));

        let header = Header {
            number: 1,
            timestamp: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };

        let mut db = InMemoryDB::default();
        db.insert_account_info(&Address::ZERO, funded());

        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);
        let pending_block = Block { header: header.clone(), body: Default::default() };
        let mut first_builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            evm,
            pending_block,
            None,
            L1FeeParams::default(),
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
            metadata: Metadata::new(header.number),
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
            L1FeeParams::default(),
            StateOverride::default(),
        );

        let cached_result = second_builder
            .execute_transaction(0, create_legacy_tx())
            .expect("cached transaction execution failed");

        assert_eq!(cached_result.execution_time_us, Some(1_234));
    }

    #[test]
    fn flashblock_tx_has_nonzero_blob_gas_used_when_jovian_active() {
        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().jovian_activated().build());
        let mut db = InMemoryDB::default();

        db.insert_account_info(&Address::ZERO, funded());

        // Seed L1 block contract slot 8 with DA footprint gas scalar at bytes [18..20] (big-endian u16).
        let da_scalar: u16 = 100;
        let mut slot_value = [0u8; 32];
        slot_value[18..20].copy_from_slice(&da_scalar.to_be_bytes());
        db.insert_account_info(&L1_BLOCK_ADDRESS, AccountInfo::default());
        db.insert_account_storage(
            &L1_BLOCK_ADDRESS,
            &DA_FOOTPRINT_GAS_SCALAR_SLOT,
            &U256::from_be_bytes(slot_value),
        );

        let header = Header {
            timestamp: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);

        let pending_block = Block { header, body: Default::default() };

        let mut builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            evm,
            pending_block,
            None,
            L1FeeParams::default(),
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
        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().build());
        let mut db = InMemoryDB::default();

        db.insert_account_info(&Address::ZERO, funded());

        let header = Header {
            timestamp: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);

        let pending_block = Block { header, body: Default::default() };

        let mut builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            evm,
            pending_block,
            None,
            L1FeeParams::default(),
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
        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().jovian_activated().build());
        let mut db = InMemoryDB::default();

        let deposit_sender: Address = address!("0x1234567890123456789012345678901234567890");
        db.insert_account_info(&deposit_sender, funded());

        // Seed L1 block contract slot 8 with DA footprint gas scalar at bytes [18..20] (big-endian u16).
        let da_scalar: u16 = 100;
        let mut slot_value = [0u8; 32];
        slot_value[18..20].copy_from_slice(&da_scalar.to_be_bytes());
        db.insert_account_info(&L1_BLOCK_ADDRESS, AccountInfo::default());
        db.insert_account_storage(
            &L1_BLOCK_ADDRESS,
            &DA_FOOTPRINT_GAS_SCALAR_SLOT,
            &U256::from_be_bytes(slot_value),
        );

        let header = Header {
            timestamp: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);

        let pending_block = Block { header, body: Default::default() };

        let mut builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            evm,
            pending_block,
            None,
            L1FeeParams::default(),
            StateOverride::default(),
        );

        let deposit_tx = base_common_consensus::TxDeposit {
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
        let envelope = BaseTxEnvelope::Deposit(sealed);
        let tx = alloy_consensus::transaction::Recovered::new_unchecked(envelope, deposit_sender);

        let result = builder.execute_transaction(0, tx).expect("deposit execution failed");

        let blob_gas_used =
            result.receipt.inner.blob_gas_used.expect("blob_gas_used should be set");
        assert_eq!(
            blob_gas_used, 0,
            "blob_gas_used should be 0 for deposit tx even when Jovian is active"
        );
    }

    /// Regression test: `execute_with_cached_data` must commit the cached `PendingState` to the EVM
    /// database so that subsequent transactions see the correct post-tx state.
    ///
    /// Without the commit, a fresh tx executed after a cached one runs against stale state
    /// (e.g. missing nonce increment, stale storage), producing logs that differ from what
    /// the final block-building executor produces. This causes a receipt root mismatch
    /// during block validation because the sequencer re-executes everything from scratch.
    #[test]
    fn cached_execute_commits_state_so_subsequent_fresh_txs_see_updated_nonce() {
        // Phase 1: execute tx A freshly to obtain the real state changes and receipt
        // that would be stored in PendingBlocks after the first flashblock round.
        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().build());
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let sender = Address::ZERO;

        let header = Header {
            number: 1,
            timestamp: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };

        let mut db = InMemoryDB::default();
        db.insert_account_info(&sender, funded());

        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);
        let pending_block = Block { header: header.clone(), body: Default::default() };
        let mut first_builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            evm,
            pending_block,
            None,
            L1FeeParams::default(),
            StateOverride::default(),
        );

        let tx_a = create_legacy_tx();
        let tx_a_hash = tx_a.tx_hash();
        let first_result =
            first_builder.execute_transaction(0, tx_a).expect("first execution failed");

        // Sanity-check: fresh execution increments the sender nonce from 0 to 1.
        let first_outcome = first_builder.finish();
        let sender_nonce_after_tx_a = first_outcome
            .cache
            .accounts
            .get(&sender)
            .and_then(Option::as_ref)
            .map(|info| info.nonce)
            .expect("sender should be in cache after tx A");

        assert_eq!(sender_nonce_after_tx_a, 1, "tx A should increment nonce to 1");

        // Phase 2: store the result of tx A in PendingBlocks, simulating what the
        // processor does after the first flashblock is built.
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
            metadata: Metadata::new(header.number),
        }]);
        pending_blocks_builder.with_transaction_sender(tx_a_hash, sender);
        pending_blocks_builder.with_receipt(tx_a_hash, first_result.receipt.clone());
        pending_blocks_builder.with_transaction_state(tx_a_hash, first_result.state.clone());
        pending_blocks_builder.with_transaction_result(tx_a_hash, first_result.result);

        let prev_pending_blocks =
            Arc::new(pending_blocks_builder.build().expect("should build pending blocks"));

        // Phase 3: build a second flashblock whose EVM starts from scratch (nonce 0).
        // tx A is now in prev_pending_blocks so execute_transaction will take the cached
        // path (execute_with_cached_data). After that call the EVM database must reflect
        // the committed state of tx A (nonce 1) so any subsequent fresh tx executes
        // against the correct state.
        let mut db2 = InMemoryDB::default();
        db2.insert_account_info(&sender, funded());
        let second_evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let second_evm = evm_config.evm_with_env(db2, second_evm_env);
        let second_pending_block = Block { header, body: Default::default() };
        let mut second_builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            second_evm,
            second_pending_block,
            Some(prev_pending_blocks),
            L1FeeParams::default(),
            StateOverride::default(),
        );

        second_builder
            .execute_transaction(0, create_legacy_tx())
            .expect("cached tx A execution failed");

        // The EVM database must now show nonce 1 for the sender, proving that
        // execute_with_cached_data committed the state before returning.
        let second_outcome = second_builder.finish();
        let sender_nonce_after_cached_tx_a = second_outcome
            .cache
            .accounts
            .get(&sender)
            .and_then(Option::as_ref)
            .map(|info| info.nonce)
            .expect("sender should be in cache after cached tx A");

        assert_eq!(
            sender_nonce_after_cached_tx_a, 1,
            "cached tx A must commit state so the sender nonce is 1 (not 0)"
        );
    }

    fn create_legacy_call(
        sender: Address,
        to: Address,
        value: U256,
        gas_limit: u64,
        nonce: u64,
    ) -> alloy_consensus::transaction::Recovered<BaseTxEnvelope> {
        let tx = alloy_consensus::TxLegacy {
            chain_id: Some(8453),
            nonce,
            gas_price: 1_000_000_000,
            gas_limit,
            to: TxKind::Call(to),
            value,
            input: Default::default(),
        };

        let envelope = BaseTxEnvelope::Legacy(Signed::new_unchecked(
            tx,
            alloy_primitives::Signature::test_signature(),
            B256::ZERO,
        ));

        alloy_consensus::transaction::Recovered::new_unchecked(envelope, sender)
    }

    /// CHAIN-4786: a Flashblock tx that touches a code-less EOA must not serialize
    /// padded empty bytecode (`0x00` / STOP) into the pending `StateOverride`.
    /// That made later `pending` `eth_call` / `eth_estimateGas` / `eth_simulateV1`
    /// see `EXTCODESIZE = 1` and take the ERC-1271 path for an EOA.
    #[test]
    fn pending_overrides_serialize_original_bytecode_not_padded_empty() {
        let sender = address!("0x1111111111111111111111111111111111111111");
        let recipient = address!("0x2222222222222222222222222222222222222222");
        let contract = address!("0x3333333333333333333333333333333333333333");
        // PUSH1 0x00 — does not end in STOP, so analysis may append padding.
        let original_contract_code = Bytes::from_static(&[0x60, 0x00]);
        let analyzed_contract = Bytecode::new_legacy(original_contract_code.clone());
        assert_eq!(analyzed_contract.original_bytes(), original_contract_code);

        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().build());
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let mut db = InMemoryDB::default();
        db.insert_account_info(&sender, funded());
        db.insert_account_info(&recipient, AccountInfo::default());
        db.insert_account_info(
            &contract,
            AccountInfo::default().with_nonce(1).with_code(analyzed_contract),
        );

        let header = Header {
            timestamp: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };
        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);
        let pending_block = Block { header, body: Default::default() };
        let mut builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            evm,
            pending_block,
            None,
            L1FeeParams::default(),
            StateOverride::default(),
        );

        builder
            .execute_transaction(0, create_legacy_call(sender, recipient, U256::from(1), 21_000, 0))
            .expect("value transfer to code-less recipient should succeed");
        builder
            .execute_transaction(1, create_legacy_call(sender, contract, U256::ZERO, 50_000, 1))
            .expect("call to contract that does not end in STOP should succeed");

        let state_overrides = builder.finish().state_overrides;

        let recipient_override = state_overrides
            .get(&recipient)
            .expect("touched recipient EOA must be present in pending overrides");
        let recipient_code = recipient_override.code.as_ref();
        assert!(
            recipient_code.is_none_or(|code| code.is_empty()),
            "code-less EOA override must be empty, not padded STOP; got {recipient_code:?}"
        );
        assert_ne!(
            recipient_code.map(Bytes::as_ref),
            Some(&[0x00][..]),
            "padded empty bytecode must not be serialized as 0x00"
        );
        if let Some(code) = recipient_code {
            assert_eq!(
                keccak256(code),
                keccak256(Bytes::new()),
                "empty-code override must hash to keccak256(\"\") so EXTCODEHASH matches a code-less EOA"
            );
        }

        let contract_override = state_overrides
            .get(&contract)
            .expect("touched contract must be present in pending overrides");
        assert_eq!(
            contract_override.code.as_ref(),
            Some(&original_contract_code),
            "contract override must store original_bytes(), not padding"
        );
    }
}
