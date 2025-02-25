use std::sync::Arc;

use crate::cache::Cache;
use alloy_consensus::transaction::TransactionMeta;
use alloy_consensus::{transaction::Recovered, transaction::TransactionInfo};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, Sealable, TxHash, B256, U256};
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
use reth_optimism_chainspec::{OpChainSpec, OP_SEPOLIA};
use reth_optimism_primitives::{OpBlock, OpReceipt, OpTransactionSigned};
use reth_optimism_rpc::OpReceiptBuilder;
use reth_rpc_eth_api::RpcReceipt;
use reth_rpc_eth_api::{
    helpers::{EthBlocks, EthState},
    RpcNodeCore,
};
use reth_rpc_eth_api::{
    helpers::{EthTransactions, FullEthApi},
    RpcBlock,
};
use serde::{Deserialize, Serialize};
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

    pub fn transform_block(&self, block: OpBlock) -> RpcBlock<Optimism> {
        let header: alloy_consensus::Header = block.header.clone();
        let transactions = block.body.transactions.to_vec();
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
    }

    pub fn transform_tx(
        &self,
        tx: Recovered<OpTransactionSigned>,
        tx_info: TransactionInfo,
    ) -> Transaction {
        let (tx, from) = tx.into_parts();
        let mut deposit_receipt_version = None;
        let mut deposit_nonce = None;

        let inner: OpTxEnvelope = tx.into();

        if inner.is_deposit() {
            let receipt = self
                .cache
                .get::<OpReceipt>(&format!("receipt:{:?}", tx_info.hash))
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

        let effective_gas_price = if inner.is_deposit() {
            // For deposits, we must always set the `gasPrice` field to 0 in rpc
            // deposit tx don't have a gas price field, but serde of `Transaction` will take care of
            // it
            0
        } else {
            base_fee
                .map(|base_fee| {
                    inner
                        .effective_tip_per_gas(base_fee as u64)
                        .unwrap_or_default()
                        + base_fee as u128
                })
                .unwrap_or_else(|| inner.max_fee_per_gas())
        };

        Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner,
                block_hash,
                block_number,
                transaction_index,
                from,
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
        chain_spec: &OpChainSpec,
    ) -> RpcReceipt<Optimism> {
        let tx = self
            .cache
            .get::<OpTransactionSigned>(&tx_hash.to_string())
            .unwrap();
        let block = self.cache.get::<OpBlock>("pending").unwrap();
        let mut l1_block_info =
            reth_optimism_evm::extract_l1_info(&block.body).expect("failed to extract l1 info");

        let meta = TransactionMeta {
            tx_hash,
            index: 0,                    // placeholder
            block_hash: B256::default(), // placeholder
            block_number: block.number,
            base_fee: block.base_fee_per_gas,
            excess_blob_gas: block.excess_blob_gas,
            timestamp: block.timestamp,
        };

        // get all receipts from cache too
        let all_receipts = self
            .cache
            .get::<Vec<OpReceipt>>("pending_receipts")
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
                if let Some(block) = self.cache.get::<OpBlock>(&number.to_string()) {
                    return Ok(Some(self.transform_block(block)));
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
        if let Some(receipt) = self
            .cache
            .get::<OpReceipt>(&format!("receipt:{:?}", tx_hash))
        {
            return Ok(Some(self.transform_receipt(
                receipt,
                tx_hash,
                &OP_SEPOLIA.as_ref(), // placeholder
            )));
        }
        info!("no receipt found in cache, using standard flow");
        EthTransactions::transaction_receipt(&self.eth_api, tx_hash)
            .await
            .map_err(Into::into)
    }

    async fn get_balance(
        &self,
        address: Address,
        block_number: Option<BlockId>,
    ) -> RpcResult<U256> {
        let block_id = block_number.unwrap_or_default();
        if block_id.is_pending() {
            if let Some(balance) = self.cache.get::<U256>(&format!("{:?}", address)) {
                return Ok(balance);
            }
            // If pending not found, use standard flow below
        }

        EthState::balance(&self.eth_api, address, block_number)
            .await
            .map_err(Into::into)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub name: String,
}

#[cfg_attr(not(test), rpc(server, namespace = "base"))]
#[cfg_attr(test, rpc(server, client, namespace = "base"))]
pub trait BaseApi {
    #[method(name = "status")]
    async fn status(&self, name: String) -> RpcResult<Status>;
}

pub struct BaseApiExt {}

#[async_trait]
impl BaseApiServer for BaseApiExt {
    async fn status(&self, name: String) -> RpcResult<Status> {
        Ok(Status { name })
    }
}
