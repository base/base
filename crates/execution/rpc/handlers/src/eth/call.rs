use crate::BaseEthApi;

impl BaseEthApi {
    #[inline]
    pub fn call_gas_limit(&self) -> u64 {
        self.inner.gas_cap()
    }

    #[inline]
    pub fn max_simulate_blocks(&self) -> u64 {
        self.inner.max_simulate_blocks()
    }

    #[inline]
    pub fn evm_memory_limit(&self) -> u64 {
        self.inner.evm_memory_limit()
    }

    #[inline]
    pub fn compute_state_root_for_eth_simulate(&self) -> bool {
        self.inner.compute_state_root_for_eth_simulate()
    }
}
