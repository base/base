use alloy_consensus::{Header, Sealed};
use alloy_primitives::{
    map::foldhash::{HashMap, HashMapExt},
    Address, BlockNumber, TxHash, B256, U256,
};
use alloy_provider::network::TransactionResponse;
use alloy_rpc_types::{state::StateOverride, BlockTransactions};
use alloy_rpc_types_eth::Header as RPCHeader;
use eyre::eyre;
use op_alloy_network::Optimism;
use op_alloy_rpc_types::{OpTransactionReceipt, Transaction};
use reth_rpc_eth_api::RpcBlock;

use crate::subscription::Flashblock;

pub struct PendingBlocksBuilder {
    flashblocks: Vec<Flashblock>,
    headers: Vec<Sealed<Header>>,
    transactions: Vec<Transaction>,
    account_balances: HashMap<Address, U256>,
    transaction_count: HashMap<Address, U256>,
    transaction_receipts: HashMap<B256, OpTransactionReceipt>,
    transactions_by_hash: HashMap<B256, Transaction>,
    state_overrides: Option<StateOverride>,
}

impl PendingBlocksBuilder {
    pub fn new() -> Self {
        Self {
            flashblocks: Vec::new(),
            headers: Vec::new(),
            transactions: Vec::new(),
            account_balances: HashMap::new(),
            transaction_count: HashMap::new(),
            transaction_receipts: HashMap::new(),
            transactions_by_hash: HashMap::new(),
            state_overrides: None,
        }
    }

    #[inline]
    pub fn with_flashblocks(&mut self, flashblocks: Vec<Flashblock>) -> &Self {
        self.flashblocks.extend(flashblocks);
        self
    }

    #[inline]
    pub fn with_header(&mut self, header: Sealed<Header>) -> &Self {
        self.headers.push(header);
        self
    }

    #[inline]
    pub(crate) fn with_transaction(&mut self, transaction: Transaction) -> &Self {
        self.transactions_by_hash
            .insert(transaction.tx_hash(), transaction.clone());
        self.transactions.push(transaction);
        self
    }

    #[inline]
    pub(crate) fn increment_nonce(&mut self, sender: Address) -> &Self {
        let zero = U256::from(0);
        let current_count = self.transaction_count.get(&sender).unwrap_or(&zero);

        _ = self
            .transaction_count
            .insert(sender, *current_count + U256::from(1));
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
            state_overrides: self.state_overrides,
        })
    }
}

#[derive(Debug, Clone)]
pub struct PendingBlocks {
    flashblocks: Vec<Flashblock>,
    headers: Vec<Sealed<Header>>,
    transactions: Vec<Transaction>,

    account_balances: HashMap<Address, U256>,
    transaction_count: HashMap<Address, U256>,
    transaction_receipts: HashMap<B256, OpTransactionReceipt>,
    transactions_by_hash: HashMap<B256, Transaction>,
    state_overrides: Option<StateOverride>,
}

impl PendingBlocks {
    pub fn latest_block_number(&self) -> BlockNumber {
        self.headers.last().unwrap().number
    }

    pub fn latest_flashblock_index(&self) -> u64 {
        self.flashblocks.last().unwrap().index
    }

    pub fn latest_header(&self) -> Sealed<Header> {
        self.headers.last().unwrap().clone()
    }

    pub fn get_flashblocks(&self) -> Vec<Flashblock> {
        self.flashblocks.clone()
    }

    pub fn get_headers(&self) -> Vec<Sealed<Header>> {
        self.headers.clone()
    }

    pub fn get_latest_block(&self, full: bool) -> RpcBlock<Optimism> {
        let header = self.latest_header();
        let block_number = header.number;
        let block_transactions: Vec<Transaction> = self
            .transactions
            .iter()
            .filter(|tx| tx.block_number.unwrap_or(0) == block_number)
            .cloned()
            .collect();

        let transactions = if full {
            BlockTransactions::Full(block_transactions.clone())
        } else {
            let tx_hashes: Vec<B256> = block_transactions
                .clone()
                .iter()
                .map(|tx| tx.tx_hash())
                .collect();
            BlockTransactions::Hashes(tx_hashes.clone())
        };

        RpcBlock::<Optimism> {
            header: RPCHeader::from_consensus(header.clone(), None, None),
            transactions,
            uncles: Vec::new(),
            withdrawals: None,
        }
    }

    pub fn get_receipt(&self, tx_hash: TxHash) -> Option<OpTransactionReceipt> {
        self.transaction_receipts.get(&tx_hash).cloned()
    }

    pub fn get_transaction_by_hash(&self, tx_hash: TxHash) -> Option<Transaction> {
        self.transactions_by_hash.get(&tx_hash).cloned()
    }

    pub fn get_transaction_count(&self, address: Address) -> U256 {
        self.transaction_count
            .get(&address)
            .cloned()
            .unwrap_or(U256::from(0))
    }

    pub fn get_balance(&self, address: Address) -> Option<U256> {
        self.account_balances.get(&address).cloned()
    }

    pub fn get_state_overrides(&self) -> Option<StateOverride> {
        self.state_overrides.clone()
    }
}
