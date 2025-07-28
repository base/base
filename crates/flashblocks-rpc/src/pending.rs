use crate::subscription::Flashblock;
use alloy_consensus::transaction::{Recovered, SignerRecoverable, TransactionMeta};
use alloy_consensus::{Header, TxReceipt};
use alloy_primitives::map::foldhash::HashMap;
use alloy_primitives::{Address, Sealable, Sealed, TxHash, B256, U256};
use alloy_provider::network::primitives::BlockTransactions;
use alloy_provider::network::TransactionResponse;
use alloy_rpc_types::TransactionTrait;
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
use alloy_rpc_types_eth::Header as RPCHeader;
use eyre::eyre;
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_network::Optimism;
use op_alloy_rpc_types::{OpTransactionReceipt, Transaction};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_primitives::{DepositReceipt, OpBlock, OpPrimitives};
use reth_optimism_rpc::OpReceiptBuilder;
use reth_rpc_convert::transaction::ConvertReceiptInput;
use reth_rpc_eth_api::RpcBlock;
use rollup_boost::ExecutionPayloadBaseV1;
use std::borrow::Cow;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct PendingBlock {
    base: ExecutionPayloadBaseV1,
    flashblocks: Vec<Flashblock>,
    chain_spec: Arc<OpChainSpec>,

    pub block_number: u64,
    pub flashblock_idx: u64,

    header: Sealed<Header>,
    account_balances: HashMap<Address, U256>,
    transaction_count: HashMap<Address, U256>,
    transaction_receipts: HashMap<B256, OpTransactionReceipt>,
    transactions_by_hash: HashMap<B256, Transaction>,
    transactions: Vec<Transaction>,
}

impl PendingBlock {
    pub fn new_block(chain_spec: Arc<OpChainSpec>, flashblock: Flashblock) -> eyre::Result<Self> {
        let base = flashblock
            .base
            .clone()
            .ok_or(eyre!("missing base flashblock"))?;

        let mut result = Self {
            chain_spec,
            header: Header::default().seal_slow(),
            block_number: flashblock.metadata.block_number,
            flashblock_idx: flashblock.index,
            base,
            transactions: Default::default(),
            flashblocks: vec![],
            account_balances: HashMap::default(),
            transaction_count: HashMap::default(),
            transaction_receipts: HashMap::default(),
            transactions_by_hash: HashMap::default(),
        };
        result.insert_data(flashblock)?;
        Ok(result)
    }

    pub fn extend_block(
        previous_cache: &PendingBlock,
        flashblock: Flashblock,
    ) -> eyre::Result<Self> {
        let mut result = previous_cache.clone();
        result.insert_data(flashblock)?;
        Ok(result)
    }

    fn insert_data(&mut self, flashblock: Flashblock) -> eyre::Result<()> {
        self.flashblocks.push(flashblock.clone());

        let transactions = self
            .flashblocks
            .iter()
            .flat_map(|flashblock| flashblock.diff.transactions.clone())
            .collect();

        let withdrawals = self
            .flashblocks
            .iter()
            .flat_map(|flashblock| flashblock.diff.withdrawals.clone())
            .collect();

        let receipt_by_hash = self
            .flashblocks
            .iter()
            .map(|flashblock| flashblock.metadata.receipts.clone())
            .fold(HashMap::default(), |mut acc, receipts| {
                acc.extend(receipts);
                acc
            });

        let execution_payload: ExecutionPayloadV3 = ExecutionPayloadV3 {
            blob_gas_used: 0,
            excess_blob_gas: 0,
            payload_inner: ExecutionPayloadV2 {
                withdrawals,
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: self.base.parent_hash,
                    fee_recipient: self.base.fee_recipient,
                    state_root: flashblock.diff.state_root,
                    receipts_root: flashblock.diff.receipts_root,
                    logs_bloom: flashblock.diff.logs_bloom,
                    prev_randao: self.base.prev_randao,
                    block_number: self.base.block_number,
                    gas_limit: self.base.gas_limit,
                    gas_used: flashblock.diff.gas_used,
                    timestamp: self.base.timestamp,
                    extra_data: self.base.extra_data.clone(),
                    base_fee_per_gas: self.base.base_fee_per_gas,
                    block_hash: flashblock.diff.block_hash,
                    transactions,
                },
            },
        };

        let block: OpBlock = execution_payload.try_into_block()?;
        let mut l1_block_info = reth_optimism_evm::extract_l1_info(&block.body)?;

        self.header = block.header.clone().seal_slow();

        self.transaction_count.clear();
        self.transaction_receipts.clear();
        self.transactions_by_hash.clear();
        self.transactions.clear();
        let mut gas_used = 0;
        let mut next_log_index = 0;

        for (idx, transaction) in block.body.transactions.iter().enumerate() {
            let sender = match transaction.recover_signer() {
                Ok(signer) => signer,
                Err(err) => return Err(err.into()),
            };

            // Transaction Count
            let zero = U256::from(0);
            let current_count = self.transaction_count.get(&sender).unwrap_or(&zero);

            _ = self
                .transaction_count
                .insert(sender, *current_count + U256::from(1));
            // End Transaction Count

            let receipt = receipt_by_hash
                .get(&transaction.tx_hash())
                .cloned()
                .ok_or(eyre!("missing receipt for {:?}", transaction.tx_hash()))?;

            let recovered_transaction = Recovered::new_unchecked(transaction.clone(), sender);
            let envelope = recovered_transaction.clone().convert::<OpTxEnvelope>();

            // Build Transaction
            let (deposit_receipt_version, deposit_nonce) = if transaction.is_deposit() {
                let deposit_receipt = receipt
                    .as_deposit_receipt()
                    .ok_or(eyre!("deposit transaction, non deposit receipt"))?;

                (
                    deposit_receipt.deposit_receipt_version,
                    deposit_receipt.deposit_nonce,
                )
            } else {
                (None, None)
            };

            let effective_gas_price = if transaction.is_deposit() {
                0
            } else {
                block
                    .base_fee_per_gas
                    .map(|base_fee| {
                        transaction
                            .effective_tip_per_gas(base_fee)
                            .unwrap_or_default()
                            + base_fee as u128
                    })
                    .unwrap_or_else(|| transaction.max_fee_per_gas())
            };

            let rpc_txn = Transaction {
                inner: alloy_rpc_types_eth::Transaction {
                    inner: envelope,
                    block_hash: Some(self.header.hash()),
                    block_number: Some(self.block_number),
                    transaction_index: Some(idx as u64),
                    effective_gas_price: Some(effective_gas_price),
                },
                deposit_nonce,
                deposit_receipt_version,
            };

            self.transactions_by_hash
                .insert(transaction.tx_hash(), rpc_txn.clone());
            self.transactions.push(rpc_txn);
            // End Transaction

            // Receipt Generation
            let meta = TransactionMeta {
                tx_hash: transaction.tx_hash(),
                index: idx as u64,
                block_hash: self.header.hash(),
                block_number: block.number,
                base_fee: block.base_fee_per_gas,
                excess_blob_gas: block.excess_blob_gas,
                timestamp: block.timestamp,
            };

            let input: ConvertReceiptInput<'_, OpPrimitives> = ConvertReceiptInput {
                receipt: Cow::Borrowed(&receipt),
                tx: Recovered::new_unchecked(transaction, sender),
                gas_used: receipt.cumulative_gas_used() - gas_used,
                next_log_index,
                meta,
            };

            let op_receipt =
                OpReceiptBuilder::new(self.chain_spec.as_ref(), input, &mut l1_block_info)?.build();

            self.transaction_receipts
                .insert(transaction.tx_hash(), op_receipt);

            gas_used = receipt.cumulative_gas_used();
            next_log_index += receipt.logs().len();
        }

        for (address, balance) in flashblock.metadata.new_account_balances {
            self.account_balances.insert(address, balance);
        }

        Ok(())
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
}
