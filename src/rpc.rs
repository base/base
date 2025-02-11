use crate::cache::Cache;
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, Sealable, TxHash, U256};
use alloy_rpc_types::BlockTransactions;
use alloy_rpc_types_eth::Header;
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use op_alloy_network::Optimism;
use op_alloy_rpc_types::Transaction;
use reth::{core::primitives::InMemorySize, providers::HeaderProvider};
use reth_optimism_primitives::{OpBlock, OpReceipt, OpTransactionSigned};
use reth_optimism_rpc::eth::OpNodeCore;
use reth_optimism_rpc::OpEthApi;
use reth_primitives::{BlockWithSenders, SealedBlockFor};
use reth_rpc_eth_api::{
    helpers::{EthBlocks, EthState},
    RpcNodeCore,
};
use reth_rpc_eth_api::{
    helpers::{EthTransactions, FullEthApi},
    EthApiTypes, RpcBlock, RpcReceipt,
};
use reth_rpc_types_compat::block::from_block;
use reth_rpc_types_compat::TransactionCompat;
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
    ) -> RpcResult<Option<OpBlock>>;

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
}

#[async_trait]
impl<Eth> EthApiOverrideServer for EthApiExt<Eth>
where
    Eth: FullEthApi<NetworkTypes = Optimism> + Send + Sync + 'static,
{
    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        _full: bool,
    ) -> RpcResult<Option<OpBlock>> {
        match number {
            BlockNumberOrTag::Pending => {
                info!("pending block by number, delegating to flashblocks");
                if let Ok(Some(block)) = self.cache.get::<OpBlock>(&number.to_string()) {
                    return Ok(Some(block));
                } else {
                    return Ok(None);
                }
            }
            _ => {
                info!("non pending block, using standard flow");
                todo!()
                // EthBlocks::rpc_block(&self.eth_api, number.into(), _full)
                //     .await
                //     .map_err(Into::into)
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
