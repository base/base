use crate::cache::Cache;
use alloy_consensus::{transaction::Recovered, transaction::TransactionInfo, Signed};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, Sealable, TxHash, U256};
use alloy_rpc_types::TransactionTrait;
use alloy_rpc_types::{BlockTransactions, Header};
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use op_alloy_consensus::{OpTxEnvelope, OpTypedTransaction};
use op_alloy_network::Optimism;
use op_alloy_rpc_types::Transaction;
use reth::{api::BlockBody, core::primitives::SignedTransaction, providers::HeaderProvider};
use reth_optimism_primitives::{OpBlock, OpReceipt, OpTransactionSigned};
use reth_primitives::RecoveredTx;
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
    ) -> RpcResult<Option<OpTransactionSigned>>;

    #[method(name = "getBalance")]
    async fn get_balance(&self, address: Address, block_number: Option<BlockId>)
        -> RpcResult<U256>;
}

#[derive(Debug)]
pub struct EthApiExt<Eth> {
    #[allow(dead_code)] // temporary until we implement the flashblocks API
    eth_api: Eth,
    cache: Cache,
}

impl<E> EthApiExt<E> {
    pub const fn new(eth_api: E, cache: Cache) -> Self {
        Self { eth_api, cache }
    }

    pub fn transform_tx(
        &self,
        tx: RecoveredTx<OpTransactionSigned>,
        tx_info: TransactionInfo,
    ) -> Transaction {
        let from = tx.signer();
        let hash = *tx.tx_hash();
        let OpTransactionSigned {
            transaction,
            signature,
            ..
        } = tx.into_tx();
        let mut deposit_receipt_version = None;
        let mut deposit_nonce = None;

        let inner = match transaction {
            OpTypedTransaction::Legacy(tx) => Signed::new_unchecked(tx, signature, hash).into(),
            OpTypedTransaction::Eip2930(tx) => Signed::new_unchecked(tx, signature, hash).into(),
            OpTypedTransaction::Eip1559(tx) => Signed::new_unchecked(tx, signature, hash).into(),
            OpTypedTransaction::Eip7702(tx) => Signed::new_unchecked(tx, signature, hash).into(),
            OpTypedTransaction::Deposit(tx) => {
                // TODO: implement deposit receipt version and nonce
                // self.eth_api
                // .provider()
                // .receipt_by_hash(hash)
                // .map_err(Into::into)?
                // .inspect(|receipt| {
                //     if let OpReceipt::Deposit(receipt) = receipt {
                //         deposit_receipt_version = receipt.deposit_receipt_version;
                //         deposit_nonce = receipt.deposit_nonce;
                //     }
                // });
                OpTxEnvelope::Deposit(tx.seal_unchecked(hash))
            }
        };

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
                        + base_fee
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
                if let Ok(Some(block)) = self.cache.get::<OpBlock>(&number.to_string()) {
                    let header: alloy_consensus::Header = block.header.clone();
                    let transactions = block.body.transactions.to_vec();
                    let transactions_with_senders = transactions
                        .into_iter()
                        .zip(block.body.recover_signers().unwrap());
                    let converted_txs = transactions_with_senders
                        .enumerate()
                        .map(|(idx, (tx, sender))| {
                            let signed_tx_ec_recovered =
                                Recovered::new_unchecked(tx.clone(), sender.clone());
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

                    let rpc_block: RpcBlock<Optimism> = RpcBlock::<Optimism> {
                        header: Header::from_consensus(header.seal_slow(), None, None),
                        transactions: BlockTransactions::Full(converted_txs),
                        uncles: Vec::new(),
                        withdrawals: None,
                    };
                    return Ok(Some(rpc_block));
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
    ) -> RpcResult<Option<OpTransactionSigned>> {
        if let Ok(Some(receipt)) = self.cache.get::<OpTransactionSigned>(&tx_hash.to_string()) {
            return Ok(Some(receipt));
        }

        todo!()

        // EthTransactions::transaction_receipt(&self.eth_api, tx_hash)
        //     .await
        //     .map_err(Into::into)
    }

    async fn get_balance(
        &self,
        address: Address,
        block_number: Option<BlockId>,
    ) -> RpcResult<U256> {
        let block_id = block_number.unwrap_or_default();
        if block_id.is_pending() {
            info!("pending tag, delegating to flashblocks");
            todo!()
        } else {
            info!("non pending block, using standard flow");
            EthState::balance(&self.eth_api, address, block_number)
                .await
                .map_err(Into::into)
        }
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
