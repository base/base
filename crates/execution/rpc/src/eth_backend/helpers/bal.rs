//! Contains RPC handler implementations specific to block access lists.

use reth_rpc_eth_api::{FromEvmError, RpcNodeCore, helpers::bal::GetBlockAccessList};
use reth_rpc_eth_types::EthApiError;

use crate::EthApi;

impl<N> GetBlockAccessList for EthApi<N>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError,
{
}
