//! Contains RPC handler implementations specific to endpoints that call/execute within evm.

use reth_rpc_eth_api::{
    FromEvmError, RpcNodeCore,
    helpers::{Call, EthCall, estimate::EstimateCall},
};
use reth_rpc_eth_types::EthApiError;

use crate::EthApi;

impl<N> EthCall for EthApi<N>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError,
{
}

impl<N> Call for EthApi<N>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError,
{
    #[inline]
    fn call_gas_limit(&self) -> u64 {
        self.inner.gas_cap()
    }

    #[inline]
    fn max_simulate_blocks(&self) -> u64 {
        self.inner.max_simulate_blocks()
    }

    #[inline]
    fn compute_state_root_for_eth_simulate(&self) -> bool {
        self.inner.compute_state_root_for_eth_simulate()
    }

    #[inline]
    fn evm_memory_limit(&self) -> u64 {
        self.inner.evm_memory_limit()
    }
}

impl<N> EstimateCall for EthApi<N>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError,
{
}
