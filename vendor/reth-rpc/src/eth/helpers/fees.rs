//! Contains RPC handler implementations for fee history.

use reth_rpc_eth_api::{
    FromEvmError, RpcNodeCore,
    helpers::{EthFees, LoadFee},
};
use reth_rpc_eth_types::{EthApiError, FeeHistoryCache, GasPriceOracle};
use reth_storage_api::ProviderHeader;

use crate::EthApi;

impl<N> EthFees for EthApi<N>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError,
{
}

impl<N> LoadFee for EthApi<N>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError,
{
    #[inline]
    fn gas_oracle(&self) -> &GasPriceOracle<Self::Provider> {
        self.inner.gas_oracle()
    }

    #[inline]
    fn fee_history_cache(&self) -> &FeeHistoryCache<ProviderHeader<N::Provider>> {
        self.inner.fee_history_cache()
    }
}
