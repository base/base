use alloy_eips::BlockNumberOrTag;
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use op_alloy_network::Optimism;
use reth_rpc_eth_api::{helpers::FullEthApi, RpcBlock};
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
    ) -> RpcResult<Option<RpcBlock<Optimism>>>;
}

#[derive(Debug)]
pub struct EthApiExt<Eth> {
    #[allow(dead_code)] // temporary until we implement the flashblocks API
    eth_api: Eth,
}

impl<E> EthApiExt<E> {
    pub const fn new(eth_api: E) -> Self {
        Self { eth_api }
    }
}

#[async_trait]
impl<Eth> EthApiOverrideServer for EthApiExt<Eth>
where
    Eth: FullEthApi + Send + Sync + 'static,
{
    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        _full: bool,
    ) -> RpcResult<Option<RpcBlock<Optimism>>> {
        match number {
            BlockNumberOrTag::Pending => {
                info!("pending block by number, delegating to flashblocks");
                todo!()
            }
            _ => {
                info!("non pending block, using standard flow");
                todo!()
            }
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
