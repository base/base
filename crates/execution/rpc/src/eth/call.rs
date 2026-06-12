use std::future::Future;

use alloy_primitives::Bytes;
use alloy_rpc_types_eth::{BlockId, state::EvmOverrides};
use reth_rpc_eth_api::{
    FromEvmError, RpcConvert, RpcTxReq,
    helpers::{Call, EthCall, SpawnBlocking, estimate::EstimateCall},
};
use tracing::{Instrument, info_span};

use crate::{BaseEthApi, BaseEthApiError, eth::RpcNodeCore};

impl<N, Rpc> EthCall for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError, Evm = N::Evm>,
{
    fn call(
        &self,
        request: RpcTxReq<<Self::RpcConvert as RpcConvert>::Network>,
        block_number: Option<BlockId>,
        overrides: EvmOverrides,
    ) -> impl Future<Output = Result<Bytes, Self::Error>> + Send {
        let block_number = block_number.unwrap_or_default();
        let has_state_overrides = overrides.state.is_some();
        let has_block_overrides = overrides.block.is_some();
        let span = info_span!(
            "eth_call",
            block = ?block_number,
            has_state_overrides,
            has_block_overrides,
        );

        async move {
            let _permit = self.acquire_owned_blocking_io().await;
            let res = Call::transact_call_at(self, request, block_number, overrides).await?;
            Self::Error::ensure_success(res.result)
        }
        .instrument(span)
    }
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
