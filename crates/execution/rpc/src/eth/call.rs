use reth_rpc_eth_api::{
    FromEvmError,
    helpers::{Call, EthCall, estimate::EstimateCall},
};

use crate::{BaseEthApi, BaseEthApiError, eth::RpcNodeCore};

impl<N> EthCall for BaseEthApi<N>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError,
{
}

impl<N> EstimateCall for BaseEthApi<N>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError,
{
}

impl<N> Call for BaseEthApi<N>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError,
{
    #[inline]
    fn call_gas_limit(&self) -> u64 {
        self.inner.eth_api.gas_cap()
    }

    #[inline]
    fn max_simulate_blocks(&self) -> u64 {
        self.inner.eth_api.max_simulate_blocks()
    }

    #[inline]
    fn evm_memory_limit(&self) -> u64 {
        self.inner.eth_api.evm_memory_limit()
    }

    #[inline]
    fn compute_state_root_for_eth_simulate(&self) -> bool {
        self.inner.eth_api.compute_state_root_for_eth_simulate()
    }
}
