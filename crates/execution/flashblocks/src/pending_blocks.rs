use std::{sync::Arc, time::Instant};

use alloy_consensus::{Header, Sealed, TxReceipt};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{
    Address, B256, BlockNumber, TxHash, U256,
    map::foldhash::{HashMap, HashMapExt},
};
use alloy_provider::network::TransactionResponse;
use alloy_rpc_types::{BlockTransactions, Withdrawal, state::StateOverride};
use alloy_rpc_types_engine::PayloadId;
use alloy_rpc_types_eth::{Filter, Header as RPCHeader, Log};
use arc_swap::Guard;
use base_common_consensus::OpTxType;
use base_common_evm::{BaseTxResult, OpHaltReason};
use base_common_flashblocks::Flashblock;
use base_common_network::Base;
use base_common_rpc_types::{BaseTransactionReceipt, Transaction};
use reth_evm::eth::EthTxResult;
use reth_revm::db::BundleState;
use reth_rpc_convert::RpcTransaction;
use reth_rpc_eth_api::{RpcBlock, RpcReceipt};
use revm::{
    context::result::ExecResultAndState, context_interface::result::ExecutionResult,
    state::EvmState,
};

use crate::{
    BuildError, PendingBlocksAPI, StateProcessorError, TransactionWithLogs, metrics::Metrics,
};

/// Builder for [`PendingBlocks`].
#[derive(Debug)]
pub struct PendingBlocksBuilder {
    flashblocks: Vec<Flashblock>,
    headers: Vec<Sealed<Header>>,

    transactions: Vec<Transaction>,
    account_balances: HashMap<Address, U256>,
    transaction_count: HashMap<Address, U256>,
    transaction_receipts: HashMap<B256, BaseTransactionReceipt>,
    transactions_by_hash: HashMap<B256, Transaction>,
    transaction_state: HashMap<B256, EvmState>,
    transaction_senders: HashMap<B256, Address>,
    state_overrides: Option<StateOverride>,
    transaction_results: HashMap<B256, ExecutionResult<OpHaltReason>>,
    execution_times: HashMap<B256, u128>,
    state_root_times: HashMap<B256, u128>,

    bundle_state: BundleState,
}

impl Default for PendingBlocksBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingBlocksBuilder {
    /// Creates a new empty builder.
    pub fn new() -> Self {
        Self {
            flashblocks: Vec::new(),
            headers: Vec::new(),
            transactions: Vec::new(),
            account_balances: HashMap::new(),
            transaction_count: HashMap::new(),
            transaction_receipts: HashMap::new(),
            transactions_by_hash: HashMap::new(),
            transaction_state: HashMap::new(),
            transaction_senders: HashMap::new(),
            transaction_results: HashMap::new(),
            execution_times: HashMap::new(),
            state_root_times: HashMap::new(),
            state_overrides: None,
            bundle_state: BundleState::default(),
        }
    }

    /// Adds flashblocks to the builder.
    #[inline]
    pub fn with_flashblocks(&mut self, flashblocks: impl IntoIterator<Item = Flashblock>) -> &Self {
        self.flashblocks.extend(flashblocks);
        self
    }

    /// Adds a header to the builder.
    #[inline]
    pub fn with_header(&mut self, header: Sealed<Header>) -> &Self {
        self.headers.push(header);
        self
    }

    /// Stores a transaction in the builder.
    #[inline]
    pub fn with_transaction(&mut self, transaction: Transaction) -> &Self {
        self.transactions_by_hash.insert(transaction.tx_hash(), transaction.clone());
        self.transactions.push(transaction);
        self
    }

    /// Stores the EVM state changes produced by a transaction.
    #[inline]
    pub fn with_transaction_state(&mut self, hash: B256, state: EvmState) -> &Self {
        self.transaction_state.insert(hash, state);
        self
    }

    /// Records the sender of a transaction.
    #[inline]
    pub fn with_transaction_sender(&mut self, hash: B256, sender: Address) -> &Self {
        self.transaction_senders.insert(hash, sender);
        self
    }

    /// Increments the pending nonce for an account.
    #[inline]
    pub fn increment_nonce(&mut self, sender: Address) -> &Self {
        let zero = U256::from(0);
        let current_count = self.transaction_count.get(&sender).unwrap_or(&zero);

        _ = self.transaction_count.insert(sender, *current_count + U256::from(1));
        self
    }

    /// Stores the receipt for a transaction.
    #[inline]
    pub fn with_receipt(&mut self, hash: B256, receipt: BaseTransactionReceipt) -> &Self {
        self.transaction_receipts.insert(hash, receipt);
        self
    }

    /// Records the balance of an account after execution.
    #[inline]
    pub fn with_account_balance(&mut self, address: Address, balance: U256) -> &Self {
        self.account_balances.insert(address, balance);
        self
    }

    /// Sets state overrides for the pending blocks.
    #[inline]
    pub fn with_state_overrides(&mut self, state_overrides: StateOverride) -> &Self {
        self.state_overrides = Some(state_overrides);
        self
    }

    /// Sets the accumulated bundle state.
    #[inline]
    pub fn with_bundle_state(&mut self, bundle_state: BundleState) -> &Self {
        self.bundle_state = bundle_state;
        self
    }

    /// Stores the execution result for a transaction.
    #[inline]
    pub fn with_transaction_result(
        &mut self,
        hash: B256,
        result: ExecutionResult<OpHaltReason>,
    ) -> &Self {
        self.transaction_results.insert(hash, result);
        self
    }

    /// Stores per-transaction EVM execution time.
    #[inline]
    pub fn with_execution_time(&mut self, hash: B256, time_us: u128) -> &Self {
        self.execution_times.insert(hash, time_us);
        self
    }

    /// Stores per-transaction state root simulation time.
    #[inline]
    pub fn with_state_root_time(&mut self, hash: B256, time_us: u128) -> &Self {
        self.state_root_times.insert(hash, time_us);
        self
    }

    /// Builds the pending blocks.
    pub fn build(self) -> Result<PendingBlocks, StateProcessorError> {
        let earliest_header = self.headers.first().cloned().ok_or(BuildError::MissingHeaders)?;
        let latest_header = self.headers.last().cloned().ok_or(BuildError::MissingHeaders)?;

        let latest_flashblock_index =
            self.flashblocks.last().map(|fb| fb.index).ok_or(BuildError::NoFlashblocks)?;

        for transaction in &self.transactions {
            let tx_hash = transaction.tx_hash();
            if !self.transaction_receipts.contains_key(&tx_hash) {
                return Err(BuildError::MissingReceipt { tx_hash }.into());
            }
        }

        Ok(PendingBlocks {
            earliest_header,
            latest_header,
            latest_flashblock_index,
            flashblocks: self.flashblocks,
            transactions: self.transactions,
            account_balances: self.account_balances,
            transaction_count: self.transaction_count,
            transaction_receipts: self.transaction_receipts,
            transactions_by_hash: self.transactions_by_hash,
            transaction_state: self.transaction_state,
            transaction_senders: self.transaction_senders,
            state_overrides: self.state_overrides,
            bundle_state: self.bundle_state,
            transaction_results: self.transaction_results,
            execution_times: self.execution_times,
            state_root_times: self.state_root_times,
        })
    }
}

/// Aggregated pending block state from flashblocks.
#[derive(Debug, Clone)]
pub struct PendingBlocks {
    earliest_header: Sealed<Header>,
    latest_header: Sealed<Header>,
    latest_flashblock_index: u64,
    flashblocks: Vec<Flashblock>,
    transactions: Vec<Transaction>,

    account_balances: HashMap<Address, U256>,
    transaction_count: HashMap<Address, U256>,
    transaction_receipts: HashMap<B256, BaseTransactionReceipt>,
    transactions_by_hash: HashMap<B256, Transaction>,
    transaction_state: HashMap<B256, EvmState>,
    transaction_senders: HashMap<B256, Address>,
    state_overrides: Option<StateOverride>,
    transaction_results: HashMap<B256, ExecutionResult<OpHaltReason>>,
    execution_times: HashMap<B256, u128>,
    state_root_times: HashMap<B256, u128>,

    bundle_state: BundleState,
}

impl PendingBlocks {
    fn transaction_with_logs(
        transaction: &Transaction,
        receipt: &BaseTransactionReceipt,
    ) -> TransactionWithLogs {
        TransactionWithLogs {
            transaction: transaction.clone(),
            logs: receipt.inner.logs().to_vec(),
            gas_used: receipt.inner.gas_used,
            status: receipt.inner.inner.status_or_post_state(),
            cumulative_gas_used: receipt.inner.inner.cumulative_gas_used(),
            contract_address: receipt.inner.contract_address,
            logs_bloom: receipt.inner.inner.logs_bloom,
        }
    }

    /// Returns the latest block number in the pending state.
    #[inline]
    pub fn latest_block_number(&self) -> BlockNumber {
        self.latest_header.number
    }

    /// Returns the canonical block number (the block before pending).
    #[inline]
    pub fn canonical_block_number(&self) -> BlockNumberOrTag {
        BlockNumberOrTag::Number(self.earliest_header.number - 1)
    }

    /// Returns the earliest block number in the pending state.
    #[inline]
    pub fn earliest_block_number(&self) -> BlockNumber {
        self.earliest_header.number
    }

    /// Returns the payload ID for the current build attempt.
    #[inline]
    pub fn payload_id(&self) -> PayloadId {
        self.flashblocks.first().map(|fb| fb.payload_id).unwrap_or_default()
    }

    /// Returns the index of the latest flashblock.
    #[inline]
    pub const fn latest_flashblock_index(&self) -> u64 {
        self.latest_flashblock_index
    }

    /// Returns the latest header.
    #[inline]
    pub fn latest_header(&self) -> Sealed<Header> {
        self.latest_header.clone()
    }

    /// Returns all flashblocks.
    pub fn get_flashblocks(&self) -> Vec<Flashblock> {
        self.flashblocks.clone()
    }

    /// Returns the EVM state for a transaction.
    pub fn get_transaction_state(&self, hash: &B256) -> Option<EvmState> {
        self.transaction_state.get(hash).cloned()
    }

    /// Returns the sender of a transaction.
    pub fn get_transaction_sender(&self, tx_hash: &B256) -> Option<Address> {
        self.transaction_senders.get(tx_hash).copied()
    }

    /// Returns a clone of the bundle state.
    ///
    /// NOTE: This clones the entire `BundleState`, which contains a `HashMap` of all touched
    /// accounts and their storage slots. The cost scales with the number of accounts and
    /// storage slots modified in the flashblock. Monitor `bundle_state_clone_duration` and
    /// `bundle_state_clone_size` metrics to track if this becomes a bottleneck.
    pub fn get_bundle_state(&self) -> BundleState {
        let size = self.bundle_state.state.len();
        let start = Instant::now();
        let cloned = self.bundle_state.clone();
        Metrics::bundle_state_clone_duration().record(start.elapsed());
        Metrics::bundle_state_clone_size().record(size as f64);
        cloned
    }

    /// Returns all transactions for a specific block number.
    pub fn get_transactions_for_block(
        &self,
        block_number: BlockNumber,
    ) -> impl Iterator<Item = &Transaction> {
        self.transactions.iter().filter(move |tx| tx.block_number.unwrap_or(0) == block_number)
    }

    /// Returns all withdrawals collected from flashblocks.
    fn get_withdrawals(&self) -> Vec<Withdrawal> {
        self.flashblocks.iter().flat_map(|fb| fb.diff.withdrawals.clone()).collect()
    }

    /// Returns the latest block, optionally with full transaction details.
    pub fn get_latest_block(&self, full: bool) -> RpcBlock<Base> {
        let header = self.latest_header();
        let block_number = header.number;
        let block_transactions: Vec<Transaction> =
            self.get_transactions_for_block(block_number).cloned().collect();

        let transactions = if full {
            BlockTransactions::Full(block_transactions)
        } else {
            let tx_hashes: Vec<B256> = block_transactions.iter().map(|tx| tx.tx_hash()).collect();
            BlockTransactions::Hashes(tx_hashes)
        };

        RpcBlock::<Base> {
            header: RPCHeader::from_consensus(header, None, None),
            transactions,
            uncles: Vec::new(),
            withdrawals: Some(self.get_withdrawals().into()),
        }
    }

    /// Returns the receipt for a transaction.
    pub fn get_receipt(&self, tx_hash: TxHash) -> Option<&BaseTransactionReceipt> {
        self.transaction_receipts.get(&tx_hash)
    }

    /// Returns the execution result for a transaction.
    pub fn get_transaction_result(&self, tx_hash: &B256) -> Option<&ExecutionResult<OpHaltReason>> {
        self.transaction_results.get(tx_hash)
    }

    /// Returns the per-transaction EVM execution time in microseconds.
    pub fn get_execution_time(&self, tx_hash: &B256) -> Option<u128> {
        self.execution_times.get(tx_hash).copied()
    }

    /// Returns the per-transaction state root simulation time in microseconds.
    pub fn get_state_root_time(&self, tx_hash: &B256) -> Option<u128> {
        self.state_root_times.get(tx_hash).copied()
    }

    /// Returns the receipt and state for a transaction.
    pub fn get_op_tx_result(&self, tx_hash: &B256) -> Option<BaseTxResult<OpHaltReason, OpTxType>> {
        let (((result, state), tx), sender) = self
            .get_transaction_result(tx_hash)
            .zip(self.get_transaction_state(tx_hash))
            .zip(self.get_transaction_by_hash(*tx_hash))
            .zip(self.get_transaction_sender(tx_hash))?;

        // Use blob_gas_used from receipt (DA footprint for Jovian) instead of
        // hardcoding 0, so that CachedExecutor correctly accumulates da_footprint_used.
        let blob_gas_used =
            self.get_receipt(*tx_hash).and_then(|r| r.inner.blob_gas_used).unwrap_or_default();

        let eth_tx_result = EthTxResult {
            result: ExecResultAndState::new(result.clone(), state),
            blob_gas_used,
            tx_type: tx.inner.inner.tx_type(),
        };

        let op_tx_result =
            BaseTxResult { inner: eth_tx_result, is_deposit: tx.inner.inner.is_deposit(), sender };

        Some(op_tx_result)
    }

    /// Returns a transaction by its hash.
    pub fn get_transaction_by_hash(&self, tx_hash: TxHash) -> Option<&Transaction> {
        self.transactions_by_hash.get(&tx_hash)
    }

    /// Returns true if the transaction hash is in the pending blocks.
    pub fn has_transaction_hash(&self, tx_hash: &B256) -> bool {
        self.transactions_by_hash.contains_key(tx_hash)
    }

    /// Returns the transaction count for an address in pending state.
    pub fn get_transaction_count(&self, address: Address) -> U256 {
        self.transaction_count.get(&address).copied().unwrap_or_else(|| U256::from(0))
    }

    /// Returns the balance for an address in pending state.
    pub fn get_balance(&self, address: Address) -> Option<U256> {
        self.account_balances.get(&address).copied()
    }

    /// Returns the state overrides for the pending state.
    pub fn get_state_overrides(&self) -> Option<StateOverride> {
        self.state_overrides.clone()
    }

    /// Returns logs matching the filter from pending state.
    pub fn get_pending_logs(&self, filter: &Filter) -> Vec<Log> {
        let mut logs = Vec::new();

        for tx in &self.transactions {
            if let Some(receipt) = self.transaction_receipts.get(&tx.tx_hash()) {
                for log in receipt.inner.logs() {
                    if filter.matches(&log.inner) {
                        logs.push(log.clone());
                    }
                }
            }
        }

        logs
    }

    /// Returns all pending transactions from flashblocks.
    pub fn get_pending_transactions(&self) -> Vec<Transaction> {
        self.transactions.clone()
    }

    /// Returns all pending transactions with their associated logs from flashblocks.
    pub fn get_pending_transactions_with_logs(&self) -> Vec<TransactionWithLogs> {
        self.transactions
            .iter()
            .filter_map(|tx| {
                self.transaction_receipts
                    .get(&tx.tx_hash())
                    .map(|receipt| Self::transaction_with_logs(tx, receipt))
            })
            .collect()
    }

    /// Returns the hashes of all pending transactions from flashblocks.
    pub fn get_pending_transaction_hashes(&self) -> Vec<B256> {
        self.transactions.iter().map(|tx| tx.tx_hash()).collect()
    }

    /// Returns the number of transactions in all flashblocks except the latest one.
    /// This is used to compute the delta (transactions only in the latest flashblock).
    fn previous_flashblocks_tx_count(&self) -> usize {
        if self.flashblocks.len() <= 1 {
            return 0;
        }
        self.flashblocks[..self.flashblocks.len() - 1]
            .iter()
            .map(|fb| fb.diff.transactions.len())
            .sum()
    }

    /// Returns logs matching the filter from only the latest flashblock (delta).
    ///
    /// Unlike `get_pending_logs`, this returns only logs from transactions
    /// that were added in the most recent flashblock, avoiding duplicates
    /// when streaming via WebSocket subscriptions.
    pub fn get_latest_flashblock_logs(&self, filter: &Filter) -> Vec<Log> {
        let prev_count = self.previous_flashblocks_tx_count();
        let mut logs = Vec::new();

        for tx in self.transactions.iter().skip(prev_count) {
            if let Some(receipt) = self.transaction_receipts.get(&tx.tx_hash()) {
                for log in receipt.inner.logs() {
                    if filter.matches(&log.inner) {
                        logs.push(log.clone());
                    }
                }
            }
        }

        logs
    }

    /// Returns transactions with their associated logs from only the latest flashblock (delta).
    ///
    /// Unlike `get_pending_transactions_with_logs`, this returns only transactions
    /// that were added in the most recent flashblock, avoiding duplicates
    /// when streaming via WebSocket subscriptions.
    pub fn get_latest_flashblock_transactions_with_logs(&self) -> Vec<TransactionWithLogs> {
        let prev_count = self.previous_flashblocks_tx_count();

        self.transactions
            .iter()
            .skip(prev_count)
            .filter_map(|tx| {
                self.transaction_receipts
                    .get(&tx.tx_hash())
                    .map(|receipt| Self::transaction_with_logs(tx, receipt))
            })
            .collect()
    }

    /// Returns transactions with their associated logs from only the latest flashblock (delta),
    /// filtered to include only transactions where at least one log matches the given filter.
    ///
    /// When a transaction matches, all of its logs are returned (not just the matching ones).
    /// This preserves full transaction context for subscribers who need complete log sets.
    pub fn get_latest_flashblock_transactions_with_logs_filtered(
        &self,
        filter: &Filter,
    ) -> Vec<TransactionWithLogs> {
        let prev_count = self.previous_flashblocks_tx_count();

        self.transactions
            .iter()
            .skip(prev_count)
            .filter_map(|tx| {
                let receipt = self.transaction_receipts.get(&tx.tx_hash())?;
                let logs = receipt.inner.logs();

                let has_match = logs.iter().any(|log| filter.matches(&log.inner));
                if !has_match {
                    return None;
                }

                Some(Self::transaction_with_logs(tx, receipt))
            })
            .collect()
    }

    /// Returns the hashes of transactions from only the latest flashblock (delta).
    ///
    /// Unlike `get_pending_transaction_hashes`, this returns only hashes
    /// of transactions that were added in the most recent flashblock,
    /// avoiding duplicates when streaming via WebSocket subscriptions.
    pub fn get_latest_flashblock_transaction_hashes(&self) -> Vec<B256> {
        let prev_count = self.previous_flashblocks_tx_count();
        self.transactions.iter().skip(prev_count).map(|tx| tx.tx_hash()).collect()
    }
}

impl PendingBlocksAPI for Guard<Option<Arc<PendingBlocks>>> {
    fn get_canonical_block_number(&self) -> BlockNumberOrTag {
        self.as_ref().map(|pb| pb.canonical_block_number()).unwrap_or(BlockNumberOrTag::Latest)
    }

    fn get_transaction_count(&self, address: Address) -> U256 {
        self.as_ref().map(|pb| pb.get_transaction_count(address)).unwrap_or_else(|| U256::from(0))
    }

    fn get_block(&self, full: bool) -> Option<RpcBlock<Base>> {
        self.as_ref().map(|pb| pb.get_latest_block(full))
    }

    fn get_transaction_receipt(
        &self,
        tx_hash: alloy_primitives::TxHash,
    ) -> Option<RpcReceipt<Base>> {
        self.as_ref().and_then(|pb| pb.get_receipt(tx_hash).cloned())
    }

    fn get_transaction_by_hash(
        &self,
        tx_hash: alloy_primitives::TxHash,
    ) -> Option<RpcTransaction<Base>> {
        self.as_ref().and_then(|pb| pb.get_transaction_by_hash(tx_hash).cloned())
    }

    fn get_balance(&self, address: Address) -> Option<U256> {
        self.as_ref().and_then(|pb| pb.get_balance(address))
    }

    fn get_state_overrides(&self) -> Option<StateOverride> {
        self.as_ref().map(|pb| pb.get_state_overrides()).unwrap_or_default()
    }

    fn get_pending_logs(&self, filter: &Filter) -> Vec<Log> {
        self.as_ref().map(|pb| pb.get_pending_logs(filter)).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{
        Header, Receipt, ReceiptWithBloom, Sealed, Signed, transaction::Recovered,
    };
    use alloy_primitives::{
        Address, B256, Bloom, Bytes, Log as PrimitiveLog, LogData, Signature, TxKind, U256,
    };
    use alloy_provider::network::TransactionResponse;
    use alloy_rpc_types_engine::PayloadId;
    use base_common_consensus::{BaseReceipt, BaseTxEnvelope, TxDeposit};
    use base_common_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
    };
    use base_common_rpc_types::{BaseTransactionReceipt, L1BlockInfo, Transaction};
    use revm::context_interface::result::ExecutionResult;

    use super::*;

    fn test_sender() -> Address {
        Address::repeat_byte(0x01)
    }

    fn test_flashblock() -> Flashblock {
        Flashblock {
            payload_id: PayloadId::default(),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::ZERO,
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number: 1,
                gas_limit: 30_000_000,
                timestamp: 1_700_000_000,
                extra_data: Bytes::default(),
                base_fee_per_gas: U256::from(1_000_000_000u64),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::default(),
                gas_used: 21000,
                block_hash: B256::ZERO,
                transactions: vec![],
                withdrawals: vec![],
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: Metadata { block_number: 1 },
        }
    }

    fn test_legacy_transaction() -> Transaction {
        Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(
                    BaseTxEnvelope::Legacy(alloy_consensus::Signed::new_unchecked(
                        alloy_consensus::TxLegacy::default(),
                        Signature::test_signature(),
                        B256::ZERO,
                    )),
                    test_sender(),
                ),
                block_hash: None,
                block_number: Some(1),
                transaction_index: Some(0),
                effective_gas_price: Some(1_000_000_000),
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
        }
    }

    /// Creates a [`Transaction`] whose `tx_hash()` equals `hash`.
    fn test_transaction_with_hash(hash: B256) -> Transaction {
        let legacy = alloy_consensus::TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 1_000_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let envelope = BaseTxEnvelope::Legacy(Signed::new_unchecked(
            legacy,
            Signature::test_signature(),
            hash,
        ));
        let recovered = Recovered::new_unchecked(envelope, Address::ZERO);
        Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: recovered,
                block_hash: Some(B256::ZERO),
                block_number: Some(1),
                transaction_index: Some(0),
                effective_gas_price: Some(1_000_000_000),
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
        }
    }

    fn test_deposit_transaction() -> Transaction {
        let deposit = TxDeposit {
            source_hash: B256::repeat_byte(0xdd),
            from: test_sender(),
            to: alloy_primitives::TxKind::Call(Address::repeat_byte(0x02)),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 21000,
            is_system_transaction: false,
            input: Bytes::new(),
        };
        Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(
                    BaseTxEnvelope::Deposit(Sealed::new_unchecked(deposit, B256::ZERO)),
                    test_sender(),
                ),
                block_hash: None,
                block_number: Some(1),
                transaction_index: Some(0),
                effective_gas_price: Some(0),
            },
            deposit_nonce: Some(42),
            deposit_receipt_version: Some(1),
        }
    }

    fn test_receipt(tx_hash: B256, blob_gas_used: Option<u64>) -> BaseTransactionReceipt {
        BaseTransactionReceipt {
            inner: alloy_rpc_types_eth::TransactionReceipt {
                inner: ReceiptWithBloom {
                    receipt: BaseReceipt::Legacy(Receipt {
                        status: alloy_consensus::Eip658Value::Eip658(true),
                        cumulative_gas_used: 21000,
                        logs: vec![],
                    }),
                    logs_bloom: Bloom::default(),
                },
                transaction_hash: tx_hash,
                transaction_index: Some(0),
                block_hash: None,
                block_number: Some(1),
                gas_used: 21000,
                effective_gas_price: 1_000_000_000,
                blob_gas_used,
                blob_gas_price: None,
                from: test_sender(),
                to: None,
                contract_address: None,
            },
            l1_block_info: L1BlockInfo::default(),
        }
    }

    /// Creates an [`BaseTransactionReceipt`] with a single log emitted from `log_address`.
    fn test_receipt_with_log(tx_hash: B256, log_address: Address) -> BaseTransactionReceipt {
        let log = Log {
            inner: PrimitiveLog {
                address: log_address,
                data: LogData::new_unchecked(vec![], Bytes::new()),
            },
            block_hash: Some(B256::ZERO),
            block_number: Some(1),
            block_timestamp: None,
            transaction_hash: Some(tx_hash),
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        };

        BaseTransactionReceipt {
            inner: alloy_rpc_types_eth::TransactionReceipt {
                inner: ReceiptWithBloom {
                    receipt: BaseReceipt::Legacy(Receipt {
                        status: alloy_consensus::Eip658Value::Eip658(true),
                        cumulative_gas_used: 21_000,
                        logs: vec![log],
                    }),
                    logs_bloom: Bloom::default(),
                },
                transaction_hash: tx_hash,
                transaction_index: Some(0),
                block_hash: Some(B256::ZERO),
                block_number: Some(1),
                gas_used: 21_000,
                effective_gas_price: 1_000_000_000,
                blob_gas_used: None,
                blob_gas_price: None,
                from: Address::ZERO,
                to: None,
                contract_address: None,
            },
            l1_block_info: Default::default(),
        }
    }

    fn test_receipt_with_subscription_fields(
        tx_hash: B256,
        log_address: Address,
        contract_address: Address,
        logs_bloom: Bloom,
    ) -> BaseTransactionReceipt {
        let mut receipt = test_receipt_with_log(tx_hash, log_address);
        receipt.inner.inner.receipt.as_receipt_mut().status =
            alloy_consensus::Eip658Value::Eip658(true);
        receipt.inner.inner.receipt.as_receipt_mut().cumulative_gas_used = 42_000;
        receipt.inner.inner.logs_bloom = logs_bloom;
        receipt.inner.contract_address = Some(contract_address);
        receipt
    }

    fn test_execution_result() -> ExecutionResult<OpHaltReason> {
        ExecutionResult::Success {
            reason: revm::context::result::SuccessReason::Stop,
            gas_used: 21000,
            gas_refunded: 0,
            logs: vec![],
            output: revm::context::result::Output::Call(Bytes::new()),
        }
    }

    fn build_pending_blocks(tx: Transaction, blob_gas_used: Option<u64>) -> (B256, PendingBlocks) {
        let tx_hash = tx.tx_hash();
        let mut builder = PendingBlocksBuilder::default();
        builder.with_flashblocks([test_flashblock()]);
        builder.with_header(Sealed::new_unchecked(Header::default(), B256::ZERO));
        builder.with_transaction(tx);
        builder.with_transaction_sender(tx_hash, test_sender());
        builder.with_transaction_state(tx_hash, Default::default());
        builder.with_transaction_result(tx_hash, test_execution_result());
        builder.with_receipt(tx_hash, test_receipt(tx_hash, blob_gas_used));
        (tx_hash, builder.build().expect("should build pending blocks"))
    }

    /// Builds a [`PendingBlocks`] with the supplied (hash, `log_address`) pairs
    /// inserted in the given order.
    fn build_pending_blocks_with_logs(entries: &[(B256, Address)]) -> PendingBlocks {
        let header = Sealed::new_unchecked(Header::default(), B256::ZERO);
        let mut builder = PendingBlocksBuilder::new();
        builder.with_flashblocks([test_flashblock()]);
        builder.with_header(header);

        for &(hash, addr) in entries {
            builder.with_transaction(test_transaction_with_hash(hash));
            builder.with_receipt(hash, test_receipt_with_log(hash, addr));
        }

        builder.build().expect("build should succeed")
    }

    #[test]
    fn get_tx_result_reconstructs_all_fields_for_legacy_tx() {
        let da_footprint = 42_000u64;
        let (tx_hash, pending_blocks) =
            build_pending_blocks(test_legacy_transaction(), Some(da_footprint));

        let result = pending_blocks.get_op_tx_result(&tx_hash).expect("should return tx result");

        assert_eq!(result.inner.blob_gas_used, da_footprint);
        assert_eq!(result.inner.tx_type, OpTxType::Legacy);
        assert!(!result.is_deposit);
        assert_eq!(result.sender, test_sender());
        assert_eq!(result.inner.result.result.gas_used(), 21000);
    }

    #[test]
    fn get_tx_result_reconstructs_all_fields_for_deposit_tx() {
        let (tx_hash, pending_blocks) = build_pending_blocks(test_deposit_transaction(), Some(0));

        let result = pending_blocks.get_op_tx_result(&tx_hash).expect("should return tx result");

        assert_eq!(result.inner.blob_gas_used, 0);
        assert_eq!(result.inner.tx_type, OpTxType::Deposit);
        assert!(result.is_deposit);
        assert_eq!(result.sender, test_sender());
        assert_eq!(result.inner.result.result.gas_used(), 21000);
    }

    #[test]
    fn get_tx_result_defaults_blob_gas_to_zero_when_receipt_field_is_none() {
        let (tx_hash, pending_blocks) = build_pending_blocks(test_legacy_transaction(), None);

        let result = pending_blocks.get_op_tx_result(&tx_hash).expect("should return tx result");

        assert_eq!(result.inner.blob_gas_used, 0);
    }

    #[test]
    fn get_tx_result_defaults_blob_gas_to_zero_without_receipt() {
        let tx = test_legacy_transaction();
        let tx_hash = tx.tx_hash();
        let mut builder = PendingBlocksBuilder::default();
        builder.with_flashblocks([test_flashblock()]);
        builder.with_header(Sealed::new_unchecked(Header::default(), B256::ZERO));
        builder.with_transaction(tx);
        builder.with_transaction_sender(tx_hash, test_sender());
        builder.with_transaction_state(tx_hash, Default::default());
        builder.with_transaction_result(tx_hash, test_execution_result());
        // Intentionally skip with_receipt to verify pending blocks reject incomplete transactions.
        let err = builder.build().expect_err("build should fail without a receipt");

        assert_eq!(err, StateProcessorError::Build(BuildError::MissingReceipt { tx_hash }));
    }

    fn test_receipt_with_log_and_topic(
        tx_hash: B256,
        log_address: Address,
        topic0: B256,
    ) -> BaseTransactionReceipt {
        let log = Log {
            inner: PrimitiveLog {
                address: log_address,
                data: LogData::new_unchecked(vec![topic0], Bytes::new()),
            },
            block_hash: Some(B256::ZERO),
            block_number: Some(1),
            block_timestamp: None,
            transaction_hash: Some(tx_hash),
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        };

        BaseTransactionReceipt {
            inner: alloy_rpc_types_eth::TransactionReceipt {
                inner: ReceiptWithBloom {
                    receipt: BaseReceipt::Legacy(Receipt {
                        status: alloy_consensus::Eip658Value::Eip658(true),
                        cumulative_gas_used: 21_000,
                        logs: vec![log],
                    }),
                    logs_bloom: Bloom::default(),
                },
                transaction_hash: tx_hash,
                transaction_index: Some(0),
                block_hash: Some(B256::ZERO),
                block_number: Some(1),
                gas_used: 21_000,
                effective_gas_price: 1_000_000_000,
                blob_gas_used: None,
                blob_gas_price: None,
                from: Address::ZERO,
                to: None,
                contract_address: None,
            },
            l1_block_info: Default::default(),
        }
    }

    fn build_pending_blocks_with_topics(entries: &[(B256, Address, B256)]) -> PendingBlocks {
        let header = Sealed::new_unchecked(Header::default(), B256::ZERO);
        let mut builder = PendingBlocksBuilder::new();
        builder.with_flashblocks([test_flashblock()]);
        builder.with_header(header);

        for &(hash, addr, topic) in entries {
            builder.with_transaction(test_transaction_with_hash(hash));
            builder.with_receipt(hash, test_receipt_with_log_and_topic(hash, addr, topic));
        }

        builder.build().expect("build should succeed")
    }

    #[test]
    fn get_pending_logs_returns_logs_in_transaction_order() {
        let hash_a = B256::with_last_byte(0xAA);
        let hash_b = B256::with_last_byte(0xBB);
        let hash_c = B256::with_last_byte(0xCC);

        let addr_a = Address::with_last_byte(0x0A);
        let addr_b = Address::with_last_byte(0x0B);
        let addr_c = Address::with_last_byte(0x0C);

        let pending =
            build_pending_blocks_with_logs(&[(hash_a, addr_a), (hash_b, addr_b), (hash_c, addr_c)]);

        let filter = Filter::default();
        let logs = pending.get_pending_logs(&filter);

        assert_eq!(logs.len(), 3, "should return one log per transaction");
        assert_eq!(logs[0].address(), addr_a);
        assert_eq!(logs[1].address(), addr_b);
        assert_eq!(logs[2].address(), addr_c);
    }

    #[test]
    fn filtered_transactions_returns_only_matching_by_address() {
        let hash_a = B256::with_last_byte(0xAA);
        let hash_b = B256::with_last_byte(0xBB);
        let hash_c = B256::with_last_byte(0xCC);

        let addr_a = Address::with_last_byte(0x0A);
        let addr_b = Address::with_last_byte(0x0B);
        let addr_c = Address::with_last_byte(0x0C);

        let pending =
            build_pending_blocks_with_logs(&[(hash_a, addr_a), (hash_b, addr_b), (hash_c, addr_c)]);

        let filter = Filter::new().address(addr_b);
        let txs = pending.get_latest_flashblock_transactions_with_logs_filtered(&filter);

        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].transaction.tx_hash(), hash_b);
        assert_eq!(txs[0].logs.len(), 1);
        assert_eq!(txs[0].logs[0].address(), addr_b);
    }

    #[test]
    fn filtered_transactions_returns_only_matching_by_topic0() {
        let hash_a = B256::with_last_byte(0xAA);
        let hash_b = B256::with_last_byte(0xBB);

        let addr = Address::with_last_byte(0x01);
        let topic_transfer = B256::with_last_byte(0x01);
        let topic_approval = B256::with_last_byte(0x02);

        let pending = build_pending_blocks_with_topics(&[
            (hash_a, addr, topic_transfer),
            (hash_b, addr, topic_approval),
        ]);

        let filter = Filter::new().event_signature(topic_transfer);
        let txs = pending.get_latest_flashblock_transactions_with_logs_filtered(&filter);

        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].transaction.tx_hash(), hash_a);
    }

    #[test]
    fn filtered_transactions_returns_all_logs_when_any_matches() {
        let hash_a = B256::with_last_byte(0xAA);
        let addr_match = Address::with_last_byte(0x0A);
        let addr_other = Address::with_last_byte(0x0B);

        let log_match = Log {
            inner: PrimitiveLog {
                address: addr_match,
                data: LogData::new_unchecked(vec![], Bytes::new()),
            },
            block_hash: Some(B256::ZERO),
            block_number: Some(1),
            block_timestamp: None,
            transaction_hash: Some(hash_a),
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        };
        let log_other = Log {
            inner: PrimitiveLog {
                address: addr_other,
                data: LogData::new_unchecked(vec![], Bytes::new()),
            },
            block_hash: Some(B256::ZERO),
            block_number: Some(1),
            block_timestamp: None,
            transaction_hash: Some(hash_a),
            transaction_index: Some(0),
            log_index: Some(1),
            removed: false,
        };

        let receipt = BaseTransactionReceipt {
            inner: alloy_rpc_types_eth::TransactionReceipt {
                inner: ReceiptWithBloom {
                    receipt: BaseReceipt::Legacy(Receipt {
                        status: alloy_consensus::Eip658Value::Eip658(true),
                        cumulative_gas_used: 42_000,
                        logs: vec![log_match, log_other],
                    }),
                    logs_bloom: Bloom::default(),
                },
                transaction_hash: hash_a,
                transaction_index: Some(0),
                block_hash: Some(B256::ZERO),
                block_number: Some(1),
                gas_used: 42_000,
                effective_gas_price: 1_000_000_000,
                blob_gas_used: None,
                blob_gas_price: None,
                from: Address::ZERO,
                to: None,
                contract_address: None,
            },
            l1_block_info: Default::default(),
        };

        let header = Sealed::new_unchecked(Header::default(), B256::ZERO);
        let mut builder = PendingBlocksBuilder::new();
        builder.with_flashblocks([test_flashblock()]);
        builder.with_header(header);
        builder.with_transaction(test_transaction_with_hash(hash_a));
        builder.with_receipt(hash_a, receipt);
        let pending = builder.build().expect("build should succeed");

        let filter = Filter::new().address(addr_match);
        let txs = pending.get_latest_flashblock_transactions_with_logs_filtered(&filter);

        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].logs.len(), 2, "should return ALL logs, not just matching");
        assert_eq!(txs[0].logs[0].address(), addr_match);
        assert_eq!(txs[0].logs[1].address(), addr_other);
    }

    #[test]
    fn filtered_transactions_returns_none_when_no_match() {
        let hash_a = B256::with_last_byte(0xAA);
        let addr_a = Address::with_last_byte(0x0A);
        let addr_unrelated = Address::with_last_byte(0xFF);

        let pending = build_pending_blocks_with_logs(&[(hash_a, addr_a)]);

        let filter = Filter::new().address(addr_unrelated);
        let txs = pending.get_latest_flashblock_transactions_with_logs_filtered(&filter);

        assert!(txs.is_empty());
    }

    #[test]
    fn filtered_transactions_populates_gas_used() {
        let hash_a = B256::with_last_byte(0xAA);
        let addr_a = Address::with_last_byte(0x0A);

        let pending = build_pending_blocks_with_logs(&[(hash_a, addr_a)]);

        let filter = Filter::new().address(addr_a);
        let txs = pending.get_latest_flashblock_transactions_with_logs_filtered(&filter);

        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].gas_used, 21_000);
    }

    #[test]
    fn unfiltered_transactions_populates_gas_used() {
        let hash_a = B256::with_last_byte(0xAA);
        let addr_a = Address::with_last_byte(0x0A);

        let pending = build_pending_blocks_with_logs(&[(hash_a, addr_a)]);

        let txs = pending.get_latest_flashblock_transactions_with_logs();

        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].gas_used, 21_000);
    }

    #[test]
    fn unfiltered_transactions_populate_receipt_fields() {
        let tx_hash = B256::with_last_byte(0xAA);
        let log_address = Address::with_last_byte(0x0A);
        let contract_address = Address::with_last_byte(0x0B);
        let logs_bloom: Bloom = [0x22; 256].into();

        let header = Sealed::new_unchecked(Header::default(), B256::ZERO);
        let mut builder = PendingBlocksBuilder::new();
        builder.with_flashblocks([test_flashblock()]);
        builder.with_header(header);
        builder.with_transaction(test_transaction_with_hash(tx_hash));
        builder.with_receipt(
            tx_hash,
            test_receipt_with_subscription_fields(
                tx_hash,
                log_address,
                contract_address,
                logs_bloom,
            ),
        );
        let pending = builder.build().expect("build should succeed");

        let txs = pending.get_latest_flashblock_transactions_with_logs();

        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].status, alloy_consensus::Eip658Value::Eip658(true));
        assert_eq!(txs[0].cumulative_gas_used, 42_000);
        assert_eq!(txs[0].contract_address, Some(contract_address));
        assert_eq!(txs[0].logs_bloom, logs_bloom);
    }

    #[test]
    fn filtered_transactions_with_combined_address_and_topic() {
        let hash_a = B256::with_last_byte(0xAA);
        let hash_b = B256::with_last_byte(0xBB);
        let hash_c = B256::with_last_byte(0xCC);

        let addr_usdc = Address::with_last_byte(0x0A);
        let addr_weth = Address::with_last_byte(0x0B);
        let topic_transfer = B256::with_last_byte(0x01);
        let topic_approval = B256::with_last_byte(0x02);

        let pending = build_pending_blocks_with_topics(&[
            (hash_a, addr_usdc, topic_transfer),
            (hash_b, addr_usdc, topic_approval),
            (hash_c, addr_weth, topic_transfer),
        ]);

        let filter = Filter::new().address(addr_usdc).event_signature(topic_transfer);
        let txs = pending.get_latest_flashblock_transactions_with_logs_filtered(&filter);

        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].transaction.tx_hash(), hash_a);
    }
}
