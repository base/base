//! Contains RPC handler implementations specific to streams subscriptions.

use reth_rpc_convert::RpcConvert;
use reth_rpc_eth_api::{RpcNodeCore, helpers::EthSubscriptions};
use reth_rpc_eth_types::EthApiError;

use crate::EthApi;

impl<N, Rpc> EthSubscriptions for EthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Error = EthApiError>,
{
}
