//! Contains RPC handler implementations specific to tracing.

use reth_rpc_eth_api::{FromEvmError, RpcNodeCore, helpers::Trace};
use reth_rpc_eth_types::EthApiError;

use crate::EthApi;

impl<N> Trace for EthApi<N>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError,
{
}
