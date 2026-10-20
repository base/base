//! Phase-0 simulation of `payer_sendTransaction` transactions.

use alloy_eips::BlockId;
use base_common_rpc_types::BaseTransactionRequest;
use base_execution_eip8130_rpc::Eip8130EthApiOverrideServer;
use jsonrpsee::{core::async_trait, types::ErrorObjectOwned};

/// Runs a transaction's first call phase before it is forwarded to the payer.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait PhaseZeroSimulator: Send + Sync + 'static {
    /// Runs `request` at the latest block, returning the simulation's JSON-RPC error when it
    /// reverts, halts, or cannot run.
    async fn simulate(&self, request: BaseTransactionRequest) -> Result<(), ErrorObjectOwned>;
}

/// [`PhaseZeroSimulator`] over the node's EIP-8130 `eth_call`.
#[derive(Debug)]
pub struct EthCallSimulator<Api> {
    api: Api,
}

impl<Api> EthCallSimulator<Api> {
    /// Simulates through `api`, such as [`base_execution_eip8130_rpc::Eip8130EthApiExt`].
    pub const fn new(api: Api) -> Self {
        Self { api }
    }
}

#[async_trait]
impl<Api: Eip8130EthApiOverrideServer> PhaseZeroSimulator for EthCallSimulator<Api> {
    async fn simulate(&self, request: BaseTransactionRequest) -> Result<(), ErrorObjectOwned> {
        self.api.call(request, Some(BlockId::latest()), None, None).await.map(drop)
    }
}
