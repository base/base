use std::sync::Arc;

use crate::cache::Cache;
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
use op_alloy_network::Optimism;
use op_alloy_rpc_types::Transaction;
use reth::{api::BlockBody, core::primitives::SignedTransaction, providers::HeaderProvider};
use reth_optimism_chainspec::{OpChainSpec, BASE_SEPOLIA};
use reth_optimism_primitives::{OpBlock, OpReceipt, OpTransactionSigned};
use reth_optimism_rpc::OpReceiptBuilder;
use reth_rpc_eth_api::helpers::EthTransactions;
use reth_rpc_eth_api::RpcReceipt;
use reth_rpc_eth_api::{helpers::FullEthApi, RpcBlock};
use reth_rpc_eth_api::{
    helpers::{EthBlocks, EthState},
    RpcNodeCore,
};
use tracing::info;

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
}

#[derive(Debug)]
pub struct EthApiExt<Eth> {
    #[allow(dead_code)] // temporary until we implement the flashblocks API
    eth_api: Eth,
    cache: Arc<Cache>,
}

impl<E> EthApiExt<E> {
    pub const fn new(eth_api: E, cache: Arc<Cache>) -> Self {
        Self { eth_api, cache }
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
                        hash: Some(*tx.tx_hash()),
                        block_hash: None,
                        block_number: Some(block.number),
                        index: Some(idx as u64),
                        base_fee: None,
                    };
                    self.transform_tx(signed_tx_ec_recovered, tx_info)
                })
                .collect();
            RpcBlock::<Optimism> {
                header: Header::from_consensus(header.seal_slow(), None, None),
                transactions: BlockTransactions::Full(converted_txs),
                uncles: Vec::new(),
                withdrawals: None,
            }
        } else {
            let tx_hashes = transactions.into_iter().map(|tx| *tx.tx_hash()).collect();
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
    ) -> Transaction {
        let tx = tx.convert::<OpTxEnvelope>();
        let mut deposit_receipt_version = None;
        let mut deposit_nonce = None;

        if tx.is_deposit() {
            let receipt = self
                .cache
                .get::<OpReceipt>(&format!("receipt:{:?}", tx_info.hash.unwrap().to_string()))
                .unwrap();
            if let OpReceipt::Deposit(receipt) = receipt {
                deposit_receipt_version = receipt.deposit_receipt_version;
                deposit_nonce = receipt.deposit_nonce;
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
            .get::<OpTransactionSigned>(&tx_hash.to_string())
            .unwrap();

        let block = self
            .cache
            .get::<OpBlock>(&format!("block:{}", block_number))
            .unwrap();
        let mut l1_block_info =
            reth_optimism_evm::extract_l1_info(&block.body).expect("failed to extract l1 info");

        let index = self
            .cache
            .get::<u64>(&format!("tx_idx:{}", &tx_hash.to_string()))
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
            .get::<Vec<OpReceipt>>(&format!("pending_receipts:{}", block_number))
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
{
    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        _full: bool,
    ) -> RpcResult<Option<RpcBlock<Optimism>>> {
        match number {
            BlockNumberOrTag::Pending => {
                info!("pending block by number, delegating to flashblocks");
                if let Some(block) = self.cache.get::<OpBlock>("pending") {
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
        let receipt = EthTransactions::transaction_receipt(&self.eth_api, tx_hash).await;

        // check if receipt is none
        if let Ok(None) = receipt {
            if let Some(receipt) = self
                .cache
                .get::<OpReceipt>(&format!("receipt:{:?}", tx_hash.to_string()))
            {
                info!("receipt found in cache");
                return Ok(Some(
                    self.transform_receipt(
                        receipt,
                        tx_hash,
                        self.cache
                            .get::<u64>(&format!("receipt_block:{:?}", tx_hash.to_string()))
                            .unwrap(),
                        BASE_SEPOLIA.as_ref(),
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
        let block_id = block_number.unwrap_or_default();
        if block_id.is_pending() {
            if let Some(balance) = self.cache.get::<U256>(address.to_string().as_str()) {
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
        let block_id = block_number.unwrap_or_default();
        if block_id.is_pending() {
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
                .get::<u64>(&format!("tx_count:{}:{}", address, latest_block_number + 1))
                .unwrap_or(0);
            return Ok(current_nonce + U256::from(tx_count));
        }

        EthState::transaction_count(&self.eth_api, address, block_number)
            .await
            .map_err(Into::into)
    }
}
