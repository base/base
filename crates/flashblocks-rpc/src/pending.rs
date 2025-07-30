use crate::subscription::Flashblock;
use alloy_consensus::Header;
use alloy_primitives::map::foldhash::HashMap;
use alloy_primitives::{Address, BlockNumber, Sealed, TxHash, B256, U256};
use alloy_provider::network::primitives::BlockTransactions;
use alloy_provider::network::TransactionResponse;
use alloy_rpc_types_eth::state::StateOverride;
use alloy_rpc_types_eth::Header as RPCHeader;
use eyre::eyre;
use op_alloy_network::Optimism;
use op_alloy_rpc_types::{OpTransactionReceipt, Transaction};
use reth_rpc_eth_api::RpcBlock;

pub struct PendingBlockBuilder {
    pub flashblocks: Vec<Flashblock>,
    pub header: Option<Sealed<Header>>,
    pub account_balances: HashMap<Address, U256>,
    pub transaction_count: HashMap<Address, U256>,
    pub transaction_receipts: HashMap<B256, OpTransactionReceipt>,
    pub transactions_by_hash: HashMap<B256, Transaction>,
    pub transactions: Vec<Transaction>,
    pub state_overrides: Option<StateOverride>,
}

impl PendingBlockBuilder {
    pub fn new() -> Self {
        Self {
            header: None,
            flashblocks: Vec::new(),
            transactions: Default::default(),
            account_balances: HashMap::default(),
            transaction_count: HashMap::default(),
            transaction_receipts: HashMap::default(),
            transactions_by_hash: HashMap::default(),
            state_overrides: None,
        }
    }

    #[inline]
    pub(crate) fn with_state_overrides(&mut self, state_overrides: StateOverride) -> &Self {
        self.state_overrides = Some(state_overrides);
        self
    }

    #[inline]
    pub(crate) fn with_header(&mut self, header: Sealed<Header>) -> &Self {
        self.header = Some(header);
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
    pub(crate) fn with_flashblocks(&mut self, flashblocks: Vec<Flashblock>) -> &Self {
        self.flashblocks = flashblocks;
        self
    }

    pub(crate) fn build(self) -> eyre::Result<PendingBlock> {
        let header = self.header.ok_or_else(|| eyre!("missing header"))?;

        if self.flashblocks.is_empty() {
            return Err(eyre!("no flashblocks"));
        }

        Ok(PendingBlock {
            header,
            account_balances: self.account_balances,
            transaction_count: self.transaction_count,
            transaction_receipts: self.transaction_receipts,
            transactions_by_hash: self.transactions_by_hash,
            transactions: self.transactions,
            flashblocks: self.flashblocks,
            state_overrides: self.state_overrides,
        })
    }
}

#[derive(Debug, Clone)]
pub struct PendingBlock {
    flashblocks: Vec<Flashblock>,
    header: Sealed<Header>,
    account_balances: HashMap<Address, U256>,
    transaction_count: HashMap<Address, U256>,
    transaction_receipts: HashMap<B256, OpTransactionReceipt>,
    transactions_by_hash: HashMap<B256, Transaction>,
    transactions: Vec<Transaction>,
    state_overrides: Option<StateOverride>,
}

impl PendingBlock {
    pub fn block_number(&self) -> BlockNumber {
        self.header.number
    }

    pub fn flashblock_index(&self) -> u64 {
        self.flashblocks.last().unwrap().index
    }

    pub fn get_flashblocks(&self) -> Vec<Flashblock> {
        self.flashblocks.clone()
    }

    pub fn get_block(&self, full: bool) -> RpcBlock<Optimism> {
        let transactions = if full {
            BlockTransactions::Full(self.transactions.clone())
        } else {
            let tx_hashes = self.transactions.iter().map(|tx| tx.tx_hash()).collect();
            BlockTransactions::Hashes(tx_hashes)
        };

        RpcBlock::<Optimism> {
            header: RPCHeader::from_consensus(self.header.clone(), None, None),
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
