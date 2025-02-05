use jsonrpsee::{core::{async_trait, RpcResult}, proc_macros::rpc};
use reth_rpc_eth_api::{helpers::{EthTransactions, FullEthApi}, RpcBlock, RpcReceipt};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::TxHash;
use op_alloy_network::Optimism;
use reth_rpc_eth_api::helpers::EthBlocks;
use serde::{Deserialize, Serialize};

use crate::cache::Cache;

#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait EthApiOverride {
    #[method(name = "getBlockByNumber")]
    async fn block_by_number(&self, number: BlockNumberOrTag, full: bool) -> RpcResult<Option<RpcBlock<Optimism>>>;

    #[method(name = "getTransactionReceipt")]
    async fn get_transaction_receipt(&self, tx_hash: TxHash) -> RpcResult<Option<RpcReceipt<Optimism>>>;
}

#[derive(Debug)]
pub struct EthApiExt<Eth> {
    eth_api: Eth,
    cache: Cache
}

impl<E> EthApiExt<E>
{
    pub const fn new(eth_api: E, cache: Cache) -> Self {
        Self { eth_api, cache }
    }
}

#[async_trait]
impl<Eth> EthApiOverrideServer for EthApiExt<Eth>
where
    Eth: FullEthApi<NetworkTypes = Optimism> + Send + Sync + 'static
{
    async fn block_by_number(&self, number: BlockNumberOrTag, _full: bool) -> RpcResult<Option<RpcBlock<Optimism>>>

    {
        match number {
            BlockNumberOrTag::Pending => {
                println!("pending block by number, delegating to flashblocks");
                todo!()
            }
            _ => {
                println!("non pending block, using standard flow");
                EthBlocks::rpc_block(&self.eth_api, number.into(), _full).await.map_err(Into::into)
            }
        }
    }
    async fn get_transaction_receipt(&self, tx_hash: TxHash) -> RpcResult<Option<RpcReceipt<Optimism>>>
    {
        if let Ok(receipt) = self.cache.get::<RpcReceipt<Optimism>>(&tx_hash.to_string()) {
            return Ok(receipt);
        }

        EthTransactions::transaction_receipt(&self.eth_api, tx_hash).await.map_err(Into::into)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub name: String
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
        Ok(Status { name: name.into() })
    }
}