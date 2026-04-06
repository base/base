//! Miner API extension for OP.

use alloy_primitives::U64;
pub use base_alloy_rpc_jsonrpsee::MinerApiExtServer;
use base_execution_payload_builder::config::{GasLimitConfig, OpDAConfig};
use jsonrpsee_core::{RpcResult, async_trait};
use tracing::debug;

base_metrics::define_metrics! {
    base_rpc.miner,
    struct = OpMinerMetrics,
    #[describe("Max DA tx size set on the miner")]
    max_da_tx_size: gauge,
    #[describe("Max DA block size set on the miner")]
    max_da_block_size: gauge,
    #[describe("Gas limit set on the miner")]
    gas_limit: gauge,
}

/// Miner API extension for OP, exposes settings for the data availability configuration via the
/// `miner_` API.
#[derive(Debug, Clone)]
pub struct OpMinerExtApi {
    da_config: OpDAConfig,
    gas_limit_config: GasLimitConfig,
}

impl OpMinerExtApi {
    /// Instantiate the miner API extension with the given, sharable data availability
    /// configuration.
    pub const fn new(da_config: OpDAConfig, gas_limit_config: GasLimitConfig) -> Self {
        Self { da_config, gas_limit_config }
    }
}

#[async_trait]
impl MinerApiExtServer for OpMinerExtApi {
    /// Handler for `miner_setMaxDASize` RPC method.
    async fn set_max_da_size(&self, max_tx_size: U64, max_block_size: U64) -> RpcResult<bool> {
        debug!(target: "rpc", max_tx_size = %max_tx_size, max_block_size = %max_block_size, "Setting max DA size");
        self.da_config.set_max_da_size(max_tx_size.to(), max_block_size.to());

        OpMinerMetrics::max_da_tx_size().set(max_tx_size.to::<u64>() as f64);
        OpMinerMetrics::max_da_block_size().set(max_block_size.to::<u64>() as f64);

        Ok(true)
    }

    /// Handler for `miner_getMaxDASize` RPC method.
    async fn get_max_da_size(&self) -> RpcResult<(U64, U64)> {
        let max_tx_size = U64::from(self.da_config.max_da_tx_size().unwrap_or(0));
        let max_block_size = U64::from(self.da_config.max_da_block_size().unwrap_or(0));
        debug!(target: "rpc", max_tx_size = %max_tx_size, max_block_size = %max_block_size, "Getting max DA size");
        Ok((max_tx_size, max_block_size))
    }

    async fn set_gas_limit(&self, gas_limit: U64) -> RpcResult<bool> {
        debug!(target: "rpc", gas_limit = %gas_limit, "Setting gas limit");
        self.gas_limit_config.set_gas_limit(gas_limit.to());
        OpMinerMetrics::gas_limit().set(gas_limit.to::<u64>() as f64);
        Ok(true)
    }
}
