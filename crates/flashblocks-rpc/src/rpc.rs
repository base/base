use std::sync::Arc;

use crate::cache::{Cache, CacheKey};
use crate::metrics::Metrics;
use alloy_consensus::transaction::TransactionMeta;
use alloy_consensus::{transaction::Recovered, transaction::TransactionInfo};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, Sealable, TxHash, U256};
use alloy_rpc_types::TransactionTrait;
use alloy_rpc_types::{BlockTransactions, Header};
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_consensus::{OpDepositReceipt, OpReceiptEnvelope};
use op_alloy_network::Optimism;
use op_alloy_rpc_types::Transaction;
use reth::providers::TransactionsProvider;
use reth::rpc::server_types::eth::TransactionSource;
use reth::{api::BlockBody, providers::HeaderProvider};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_primitives::{OpBlock, OpReceipt, OpTransactionSigned};
use reth_optimism_rpc::OpReceiptBuilder;
use reth_rpc_eth_api::helpers::EthTransactions;
use reth_rpc_eth_api::{helpers::FullEthApi, RpcBlock};
use reth_rpc_eth_api::{
    helpers::{EthBlocks, EthState},
    RpcNodeCore,
};
use reth_rpc_eth_api::{RpcReceipt, RpcTransaction};
use tracing::{debug, error, info};

#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait EthApiOverride {
    #[method(name = "getBlockByNumber")]
    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<RpcBlock<op_alloy_network::Optimism>>>;

    #[method(name = "getTransactionReceipt")]
    async fn get_transaction_receipt(
        &self,
        tx_hash: TxHash,
    ) -> RpcResult<Option<RpcReceipt<Optimism>>>;

    #[method(name = "getBalance")]
    async fn get_balance(&self, address: Address, block_number: Option<BlockId>)
        -> RpcResult<U256>;

    #[method(name = "getTransactionCount")]
    async fn get_transaction_count(
        &self,
        address: Address,
        block_number: Option<BlockId>,
    ) -> RpcResult<U256>;

    #[method(name = "getTransactionByHash")]
    async fn transaction_by_hash(
        &self,
        tx_hash: TxHash,
    ) -> RpcResult<Option<RpcTransaction<Optimism>>>;
}

#[derive(Debug)]
pub struct EthApiExt<Eth> {
    #[allow(dead_code)] // temporary until we implement the flashblocks API
    eth_api: Eth,
    cache: Arc<Cache>,
    metrics: Metrics,
    chain_spec: Arc<OpChainSpec>,
}

impl<E> EthApiExt<E> {
    pub fn new(eth_api: E, cache: Arc<Cache>, chain_spec: Arc<OpChainSpec>) -> Self {
        Self {
            eth_api,
            cache,
            metrics: Metrics::default(),
            chain_spec,
        }
    }

    pub fn transform_block(&self, block: OpBlock, full: bool) -> RpcBlock<Optimism> {
        let header: alloy_consensus::Header = block.header.clone();
        let transactions = block.body.transactions.to_vec();

        if full {
            let transactions_with_senders = transactions
                .into_iter()
                .zip(block.body.recover_signers().unwrap());
            let converted_txs = transactions_with_senders
                .enumerate()
                .map(|(idx, (tx, sender))| {
                    let signed_tx_ec_recovered = Recovered::new_unchecked(tx.clone(), sender);
                    let tx_info = TransactionInfo {
                        hash: Some(tx.tx_hash()),
                        block_hash: Some(block.header.hash_slow()),
                        block_number: Some(block.number),
                        index: Some(idx as u64),
                        base_fee: block.base_fee_per_gas,
                    };
                    self.transform_tx(signed_tx_ec_recovered, tx_info, None)
                })
                .collect();
            RpcBlock::<Optimism> {
                header: Header::from_consensus(header.seal_slow(), None, None),
                transactions: BlockTransactions::Full(converted_txs),
                uncles: Vec::new(),
                withdrawals: None,
            }
        } else {
            let tx_hashes = transactions.into_iter().map(|tx| tx.tx_hash()).collect();
            RpcBlock::<Optimism> {
                header: Header::from_consensus(header.seal_slow(), None, None),
                transactions: BlockTransactions::Hashes(tx_hashes),
                uncles: Vec::new(),
                withdrawals: None,
            }
        }
    }

    pub fn transform_tx(
        &self,
        tx: Recovered<OpTransactionSigned>,
        tx_info: TransactionInfo,
        deposit_receipt: Option<OpDepositReceipt>,
    ) -> Transaction {
        let tx = tx.convert::<OpTxEnvelope>();
        let mut deposit_receipt_version = None;
        let mut deposit_nonce = None;

        if tx.is_deposit() {
            if let Some(receipt) = deposit_receipt {
                deposit_receipt_version = receipt.deposit_receipt_version;
                deposit_nonce = receipt.deposit_nonce;
            } else {
                let cached_receipt = self
                    .cache
                    .get::<OpReceipt>(&CacheKey::Receipt(tx_info.hash.unwrap()))
                    .unwrap();

                if let OpReceipt::Deposit(receipt) = cached_receipt {
                    deposit_receipt_version = receipt.deposit_receipt_version;
                    deposit_nonce = receipt.deposit_nonce;
                }
            }
        }

        let TransactionInfo {
            block_hash,
            block_number,
            index: transaction_index,
            base_fee,
            ..
        } = tx_info;

        let effective_gas_price = if tx.is_deposit() {
            // For deposits, we must always set the `gasPrice` field to 0 in rpc
            // deposit tx don't have a gas price field, but serde of `Transaction` will take care of
            // it
            0
        } else {
            base_fee
                .map(|base_fee| {
                    tx.effective_tip_per_gas(base_fee).unwrap_or_default() + base_fee as u128
                })
                .unwrap_or_else(|| tx.max_fee_per_gas())
        };

        Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: tx,
                block_hash,
                block_number,
                transaction_index,
                effective_gas_price: Some(effective_gas_price),
            },
            deposit_nonce,
            deposit_receipt_version,
        }
    }

    pub fn transform_receipt(
        &self,
        receipt: OpReceipt,
        tx_hash: TxHash,
        block_number: u64,
        chain_spec: &OpChainSpec,
    ) -> RpcReceipt<Optimism> {
        let tx = self
            .cache
            .get::<OpTransactionSigned>(&CacheKey::Transaction(tx_hash))
            .unwrap();

        let block = self
            .cache
            .get::<OpBlock>(&CacheKey::Block(block_number))
            .unwrap();
        let mut l1_block_info =
            reth_optimism_evm::extract_l1_info(&block.body).expect("failed to extract l1 info");

        let index = self
            .cache
            .get::<u64>(&CacheKey::TransactionIndex(tx_hash))
            .unwrap();
        let meta = TransactionMeta {
            tx_hash,
            index,
            block_hash: block.header.hash_slow(),
            block_number: block.number,
            base_fee: block.base_fee_per_gas,
            excess_blob_gas: block.excess_blob_gas,
            timestamp: block.timestamp,
        };

        // get all receipts from cache too
        let all_receipts = self
            .cache
            .get::<Vec<OpReceipt>>(&CacheKey::PendingReceipts(block_number))
            .unwrap();

        OpReceiptBuilder::new(
            chain_spec,
            &tx,
            meta,
            &receipt,
            &all_receipts,
            &mut l1_block_info,
        )
        .expect("failed to build receipt")
        .build()
    }
}

#[async_trait]
impl<Eth> EthApiOverrideServer for EthApiExt<Eth>
where
    Eth: FullEthApi<NetworkTypes = Optimism> + Send + Sync + 'static,
    Eth: RpcNodeCore,
    <Eth as RpcNodeCore>::Provider: HeaderProvider<Header = alloy_consensus::Header>,
    <Eth as RpcNodeCore>::Provider: TransactionsProvider<Transaction = OpTransactionSigned>,
{
    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        _full: bool,
    ) -> RpcResult<Option<RpcBlock<Optimism>>> {
        debug!("block_by_number: {:?}", number);
        match number {
            BlockNumberOrTag::Pending => {
                debug!("pending block by number, delegating to flashblocks");
                self.metrics.get_block_by_number.increment(1);
                if let Some(block) = self.cache.get::<OpBlock>(&CacheKey::PendingBlock) {
                    return Ok(Some(self.transform_block(block, _full)));
                } else {
                    return Ok(None);
                }
            }
            _ => {
                info!("non pending block, using standard flow");
                EthBlocks::rpc_block(&self.eth_api, number.into(), _full)
                    .await
                    .map_err(Into::into)
            }
        }
    }

    async fn get_transaction_receipt(
        &self,
        tx_hash: TxHash,
    ) -> RpcResult<Option<RpcReceipt<Optimism>>> {
        debug!("get_transaction_receipt: {:?}", tx_hash);
        let receipt = EthTransactions::transaction_receipt(&self.eth_api, tx_hash).await;

        // check if receipt is none
        if let Ok(None) = receipt {
            if let Some(receipt) = self.cache.get::<OpReceipt>(&CacheKey::Receipt(tx_hash)) {
                self.metrics.get_transaction_receipt.increment(1);
                return Ok(Some(
                    self.transform_receipt(
                        receipt,
                        tx_hash,
                        self.cache
                            .get::<u64>(&CacheKey::ReceiptBlock(tx_hash))
                            .unwrap(),
                        self.chain_spec.as_ref(),
                    ),
                ));
            }
        }

        return receipt.map_err(Into::into);
    }

    async fn get_balance(
        &self,
        address: Address,
        block_number: Option<BlockId>,
    ) -> RpcResult<U256> {
        debug!("get_balance: {:?}", address);
        let block_id = block_number.unwrap_or_default();
        if block_id.is_pending() {
            self.metrics.get_balance.increment(1);
            if let Some(balance) = self.cache.get::<U256>(&CacheKey::AccountBalance(address)) {
                return Ok(balance);
            }
            // If pending not found, use standard flow below
        }

        EthState::balance(&self.eth_api, address, block_number)
            .await
            .map_err(Into::into)
    }

    async fn get_transaction_count(
        &self,
        address: Address,
        block_number: Option<BlockId>,
    ) -> RpcResult<U256> {
        debug!("get_transaction_count: {:?}", address);
        let block_id = block_number.unwrap_or_default();
        if block_id.is_pending() {
            self.metrics.get_transaction_count.increment(1);
            let current_nonce = EthState::transaction_count(
                &self.eth_api,
                address,
                Some(BlockId::Number(BlockNumberOrTag::Latest)),
            )
            .await
            .map_err(Into::into)?;

            // get the current latest block number
            let latest_block_header =
                EthBlocks::rpc_block_header(&self.eth_api, BlockNumberOrTag::Latest.into())
                    .await
                    .map_err(Into::into)?;

            // Check if we have a block header
            let latest_block_number = if let Some(header) = latest_block_header {
                header.number
            } else {
                // If there's no latest block, return the current nonce without additions
                return Ok(current_nonce);
            };

            let tx_count = self
                .cache
                .get::<u64>(&CacheKey::TransactionCount {
                    address,
                    block_number: latest_block_number + 1,
                })
                .unwrap_or(0);

            return Ok(current_nonce + U256::from(tx_count));
        }

        EthState::transaction_count(&self.eth_api, address, block_number)
            .await
            .map_err(Into::into)
    }

    async fn transaction_by_hash(
        &self,
        tx_hash: TxHash,
    ) -> RpcResult<Option<RpcTransaction<Optimism>>> {
        debug!("transaction_by_hash: {:?}", tx_hash);
        let tx = EthTransactions::transaction_by_hash(&self.eth_api, tx_hash)
            .await
            .map_err(Into::into)?;

        // Process the result without using map() and transpose()
        if let Some(tx_source) = tx {
            match tx_source {
                TransactionSource::Pool(tx) => {
                    // Convert the pool transaction
                    let tx_info = TransactionInfo::default();
                    Ok(Some(self.transform_tx(tx, tx_info, None)))
                }
                TransactionSource::Block {
                    transaction,
                    index,
                    block_hash,
                    block_number,
                    base_fee,
                } => {
                    // Convert the block transaction
                    let tx_info = TransactionInfo {
                        hash: Some(tx_hash),
                        index: Some(index),
                        block_hash: Some(block_hash),
                        block_number: Some(block_number),
                        base_fee,
                    };
                    // preload transaction receipt if it's a deposit transaction
                    if transaction.is_deposit() {
                        let receipt =
                            EthTransactions::transaction_receipt(&self.eth_api, tx_hash).await;

                        match receipt {
                            Ok(Some(txn_receipt)) => {
                                let envelope: OpReceiptEnvelope = txn_receipt.into();

                                if let OpReceiptEnvelope::Deposit(deposit_receipt) = envelope {
                                    return Ok(Some(self.transform_tx(
                                        transaction,
                                        tx_info,
                                        Some(deposit_receipt.receipt),
                                    )));
                                }
                            }
                            // Ok(None) or Err(e)
                            Ok(None) => {
                                error!(
                                    "could not find receipt for block transaction: {:?}",
                                    tx_hash
                                );
                                return Ok(None);
                            }
                            Err(e) => {
                                error!(
                                    "unable to fetch receipt for block transaction: {:?} error: {:?}",
                                    tx_hash, e
                                );
                                return Err(e.into());
                            }
                        }
                    }

                    Ok(Some(self.transform_tx(transaction, tx_info, None)))
                }
            }
        } else {
            // Handle cache lookup for transactions not found in the main lookup
            if let Some(tx) = self
                .cache
                .get::<OpTransactionSigned>(&CacheKey::Transaction(tx_hash))
            {
                let sender = self
                    .cache
                    .get::<Address>(&CacheKey::TransactionSender(tx_hash))
                    .unwrap();
                let block_number = self
                    .cache
                    .get::<u64>(&CacheKey::TransactionBlockNumber(tx_hash))
                    .unwrap();
                let block = self
                    .cache
                    .get::<OpBlock>(&CacheKey::Block(block_number))
                    .unwrap();
                let index = self
                    .cache
                    .get::<u64>(&CacheKey::TransactionIndex(tx_hash))
                    .unwrap();
                let tx_info = TransactionInfo {
                    hash: Some(tx.tx_hash()),
                    block_hash: Some(block.header.hash_slow()),
                    block_number: Some(block.number),
                    index: Some(index),
                    base_fee: block.base_fee_per_gas,
                };
                let tx = Recovered::new_unchecked(tx, sender);
                Ok(Some(self.transform_tx(tx, tx_info, None)))
            } else {
                Ok(None)
            }
        }
    }
}
