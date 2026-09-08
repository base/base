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
        self.inner.gas_cap()
    }

    #[inline]
    fn max_simulate_blocks(&self) -> u64 {
        self.inner.max_simulate_blocks()
    }

    #[inline]
    fn evm_memory_limit(&self) -> u64 {
        self.inner.evm_memory_limit()
    }

    #[inline]
    fn compute_state_root_for_eth_simulate(&self) -> bool {
        self.inner.compute_state_root_for_eth_simulate()
    }
}
