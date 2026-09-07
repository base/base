//! Builds an RPC receipt response w.r.t. data layout of network.

use reth_rpc_eth_api::{FromEvmError, RpcNodeCore, helpers::LoadReceipt};
use reth_rpc_eth_types::EthApiError;

use crate::EthApi;

impl<N> LoadReceipt for EthApi<N>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError,
{
}
