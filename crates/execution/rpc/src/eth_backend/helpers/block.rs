//! Contains RPC handler implementations specific to blocks.

use reth_rpc_eth_api::{
    FromEvmError, RpcNodeCore,
    helpers::{EthBlocks, LoadBlock, LoadPendingBlock},
};
use reth_rpc_eth_types::EthApiError;

use crate::EthApi;

impl<N> EthBlocks for EthApi<N>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError,
{
}

impl<N> LoadBlock for EthApi<N>
where
    Self: LoadPendingBlock,
    N: RpcNodeCore,
{
}
