use std::sync::Arc;

use alloy_consensus::{Header, Sealed};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{
    Address, B256, BlockNumber, TxHash, U256,
    map::foldhash::{HashMap, HashMapExt},
};
use alloy_provider::network::TransactionResponse;
use alloy_rpc_types::{BlockTransactions, state::StateOverride};
use alloy_rpc_types_eth::{Filter, Header as RPCHeader, Log};
use arc_swap::Guard;
use eyre::eyre;
use op_alloy_network::Optimism;
use op_alloy_rpc_types::{OpTransactionReceipt, Transaction};
use reth::revm::{db::Cache, state::EvmState};
use reth_rpc_convert::RpcTransaction;
use reth_rpc_eth_api::{RpcBlock, RpcReceipt};

use crate::{Flashblock, PendingBlocksAPI};

/// Builder for [`PendingBlocks`].
#[derive(Debug)]
pub struct PendingBlocksBuilder {
    flashblocks: Vec<Flashblock>,
    headers: Vec<Sealed<Header>>,

    transactions: Vec<Transaction>,
    account_balances: HashMap<Address, U256>,
    transaction_count: HashMap<Address, U256>,
    transaction_receipts: HashMap<B256, OpTransactionReceipt>,
    transactions_by_hash: HashMap<B256, Transaction>,
    transaction_state: HashMap<B256, EvmState>,
    transaction_senders: HashMap<B256, Address>,
    state_overrides: Option<StateOverride>,

    db_cache: Cache,
}

impl PendingBlocksBuilder {
    pub(crate) fn new() -> Self {
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
            state_overrides: None,
            db_cache: Cache::default(),
        }
    }

    #[inline]
    pub(crate) fn with_flashblocks(&mut self, flashblocks: Vec<Flashblock>) -> &Self {
        self.flashblocks.extend(flashblocks);
        self
    }

    #[inline]
    pub(crate) fn with_header(&mut self, header: Sealed<Header>) -> &Self {
        self.headers.push(header);
        self
    }

    #[inline]
    pub(crate) fn with_transaction(&mut self, transaction: Transaction) -> &Self {
        self.transactions_by_hash.insert(transaction.tx_hash(), transaction.clone());
        self.transactions.push(transaction);
        self
    }

    #[inline]
    pub(crate) fn with_db_cache(&mut self, cache: Cache) -> &Self {
        self.db_cache = cache;
        self
    }

    #[inline]
    pub(crate) fn with_transaction_state(&mut self, hash: B256, state: EvmState) -> &Self {
        self.transaction_state.insert(hash, state);
        self
    }

    #[inline]
    pub(crate) fn with_transaction_sender(&mut self, hash: B256, sender: Address) -> &Self {
        self.transaction_senders.insert(hash, sender);
        self
    }

    #[inline]
    pub(crate) fn increment_nonce(&mut self, sender: Address) -> &Self {
        let zero = U256::from(0);
        let current_count = self.transaction_count.get(&sender).unwrap_or(&zero);

        _ = self.transaction_count.insert(sender, *current_count + U256::from(1));
        self
    }

    #[inline]
    pub(crate) fn with_receipt(&mut self, hash: B256, receipt: OpTransactionReceipt) -> &Self {
        self.transaction_receipts.insert(hash, receipt);
        self
    }

    #[inline]
    pub(crate) fn with_account_balance(&mut self, address: Address, balance: U256) -> &Self {
        self.account_balances.insert(address, balance);
        self
    }

    #[inline]
    pub(crate) fn with_state_overrides(&mut self, state_overrides: StateOverride) -> &Self {
        self.state_overrides = Some(state_overrides);
        self
    }

    pub(crate) fn build(self) -> eyre::Result<PendingBlocks> {
        if self.headers.is_empty() {
            return Err(eyre!("missing headers"));
        }

        if self.flashblocks.is_empty() {
            return Err(eyre!("no flashblocks"));
        }

        Ok(PendingBlocks {
            flashblocks: self.flashblocks,
            headers: self.headers,
            transactions: self.transactions,
            account_balances: self.account_balances,
            transaction_count: self.transaction_count,
            transaction_receipts: self.transaction_receipts,
            transactions_by_hash: self.transactions_by_hash,
            transaction_state: self.transaction_state,
            transaction_senders: self.transaction_senders,
            state_overrides: self.state_overrides,
            db_cache: self.db_cache,
        })
    }
}

/// Aggregated pending block state from flashblocks.
#[derive(Debug, Clone)]
pub struct PendingBlocks {
    flashblocks: Vec<Flashblock>,
    headers: Vec<Sealed<Header>>,
    transactions: Vec<Transaction>,

    account_balances: HashMap<Address, U256>,
    transaction_count: HashMap<Address, U256>,
    transaction_receipts: HashMap<B256, OpTransactionReceipt>,
    transactions_by_hash: HashMap<B256, Transaction>,
    transaction_state: HashMap<B256, EvmState>,
    transaction_senders: HashMap<B256, Address>,
    state_overrides: Option<StateOverride>,

    db_cache: Cache,
}

impl PendingBlocks {
    /// Returns the latest block number in the pending state.
    pub fn latest_block_number(&self) -> BlockNumber {
        self.headers.last().unwrap().number
    }

    /// Returns the canonical block number (the block before pending).
    pub fn canonical_block_number(&self) -> BlockNumberOrTag {
        BlockNumberOrTag::Number(self.headers.first().unwrap().number - 1)
    }

    /// Returns the earliest block number in the pending state.
    pub fn earliest_block_number(&self) -> BlockNumber {
        self.headers.first().unwrap().number
    }

    /// Returns the index of the latest flashblock.
    pub fn latest_flashblock_index(&self) -> u64 {
        self.flashblocks.last().unwrap().index
    }

    /// Returns the latest header.
    pub fn latest_header(&self) -> Sealed<Header> {
        self.headers.last().unwrap().clone()
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
        self.transaction_senders.get(tx_hash).cloned()
    }

    /// Returns the database cache.
    pub fn get_db_cache(&self) -> Cache {
        self.db_cache.clone()
    }

    /// Returns all transactions for a specific block number.
    pub fn get_transactions_for_block(&self, block_number: BlockNumber) -> Vec<Transaction> {
        self.transactions
            .iter()
            .filter(|tx| tx.block_number.unwrap_or(0) == block_number)
            .cloned()
            .collect()
    }

    /// Returns the latest block, optionally with full transaction details.
    pub fn get_latest_block(&self, full: bool) -> RpcBlock<Optimism> {
        let header = self.latest_header();
        let block_number = header.number;
        let block_transactions: Vec<Transaction> = self.get_transactions_for_block(block_number);

        let transactions = if full {
            BlockTransactions::Full(block_transactions)
        } else {
            let tx_hashes: Vec<B256> = block_transactions.iter().map(|tx| tx.tx_hash()).collect();
            BlockTransactions::Hashes(tx_hashes)
        };

        RpcBlock::<Optimism> {
            header: RPCHeader::from_consensus(header, None, None),
            transactions,
            uncles: Vec::new(),
            withdrawals: None,
        }
    }

    /// Returns the receipt for a transaction.
    pub fn get_receipt(&self, tx_hash: TxHash) -> Option<OpTransactionReceipt> {
        self.transaction_receipts.get(&tx_hash).cloned()
    }

    /// Returns a transaction by its hash.
    pub fn get_transaction_by_hash(&self, tx_hash: TxHash) -> Option<Transaction> {
        self.transactions_by_hash.get(&tx_hash).cloned()
    }

    /// Returns the transaction count for an address in pending state.
    pub fn get_transaction_count(&self, address: Address) -> U256 {
        self.transaction_count.get(&address).cloned().unwrap_or(U256::from(0))
    }

    /// Returns the balance for an address in pending state.
    pub fn get_balance(&self, address: Address) -> Option<U256> {
        self.account_balances.get(&address).cloned()
    }

    /// Returns the state overrides for the pending state.
    pub fn get_state_overrides(&self) -> Option<StateOverride> {
        self.state_overrides.clone()
    }

    /// Returns logs matching the filter from pending state.
    pub fn get_pending_logs(&self, filter: &Filter) -> Vec<Log> {
        let mut logs = Vec::new();

        // Iterate through all transaction receipts in pending state
        for receipt in self.transaction_receipts.values() {
            for log in receipt.inner.logs() {
                if filter.matches(&log.inner) {
                    logs.push(log.clone());
                }
            }
        }

        logs
    }
}

impl PendingBlocksAPI for Guard<Option<Arc<PendingBlocks>>> {
    fn get_canonical_block_number(&self) -> BlockNumberOrTag {
        self.as_ref().map(|pb| pb.canonical_block_number()).unwrap_or(BlockNumberOrTag::Latest)
    }

    fn get_transaction_count(&self, address: Address) -> U256 {
        self.as_ref().map(|pb| pb.get_transaction_count(address)).unwrap_or_else(|| U256::from(0))
    }

    fn get_block(&self, full: bool) -> Option<RpcBlock<Optimism>> {
        self.as_ref().map(|pb| pb.get_latest_block(full))
    }

    fn get_transaction_receipt(
        &self,
        tx_hash: alloy_primitives::TxHash,
    ) -> Option<RpcReceipt<Optimism>> {
        self.as_ref().and_then(|pb| pb.get_receipt(tx_hash))
    }

    fn get_transaction_by_hash(
        &self,
        tx_hash: alloy_primitives::TxHash,
    ) -> Option<RpcTransaction<Optimism>> {
        self.as_ref().and_then(|pb| pb.get_transaction_by_hash(tx_hash))
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
