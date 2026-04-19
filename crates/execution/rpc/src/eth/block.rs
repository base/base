//! Loads and formats Base block RPC response.

use reth_rpc_eth_api::{
    FromEvmError, RpcConvert,
    helpers::{EthBlocks, LoadBlock},
};

use crate::{BaseEthApi, BaseEthApiError, eth::RpcNodeCore};

impl<N, Rpc> EthBlocks for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError>,
{
}

impl<N, Rpc> LoadBlock for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError>,
{
}
