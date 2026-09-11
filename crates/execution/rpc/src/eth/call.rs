use alloy_consensus::BlockHeader;
use base_common_chains::BaseUpgrade;
use base_protocol::BaseTimeUpdateTx;
use reth_chainspec::{ChainSpecProvider, EthChainSpec, Hardforks};
use reth_rpc_eth_api::{
    FromEvmError, RpcConvert,
    helpers::{Call, EthCall, estimate::EstimateCall},
};

use crate::{BaseEthApi, BaseEthApiError, eth::RpcNodeCore};

impl<N, Rpc> EthCall for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError, Evm = N::Evm>,
{
}

impl<N, Rpc> EstimateCall for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError, Evm = N::Evm>,
{
}

impl<N, Rpc> Call for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError, Evm = N::Evm>,
{
    fn next_simulate_block_timestamp(
        &self,
        _parent_number: u64,
        parent_timestamp: u64,
        block_number: u64,
        timestamp_increment: u64,
    ) -> u64 {
        let chain_spec = self.provider().chain_spec();
        let Some(denim_timestamp) = chain_spec.fork(BaseUpgrade::Denim).as_timestamp() else {
            return parent_timestamp.saturating_add(timestamp_increment);
        };

        let genesis = chain_spec.genesis_header();
        let legacy_block_time = timestamp_increment.max(1);
        let activation_offset =
            denim_timestamp.saturating_sub(genesis.timestamp()).div_ceil(legacy_block_time);
        let activation_block = genesis.number().saturating_add(activation_offset);

        if block_number < activation_block {
            return parent_timestamp.saturating_add(timestamp_increment);
        }

        let activation_timestamp =
            genesis.timestamp().saturating_add(activation_offset.saturating_mul(legacy_block_time));
        let blocks_since_activation = block_number.saturating_sub(activation_block);
        activation_timestamp.saturating_add(
            blocks_since_activation
                .saturating_mul(u64::from(BaseTimeUpdateTx::BLOCK_INTERVAL_MILLIS))
                / 1_000,
        )
    }

    fn is_simulate_block_timestamp_valid(&self, timestamp: u64, parent_timestamp: u64) -> bool {
        let denim = self.provider().chain_spec().fork(BaseUpgrade::Denim);
        timestamp > parent_timestamp
            || (timestamp == parent_timestamp
                && denim.active_at_timestamp(timestamp)
                && denim.active_at_timestamp(parent_timestamp))
    }

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
