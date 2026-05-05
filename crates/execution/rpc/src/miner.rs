//! Miner API extension for Base.

use alloy_primitives::U64;
use base_execution_payload_builder::config::{BaseDAConfig, GasLimitConfig};
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::{RpcResult, async_trait};
use tracing::debug;

/// Base API extension for controlling the miner.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "miner"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "miner"))]
pub trait MinerApiExt {
    /// Sets the maximum data availability size of any transaction allowed in a block, and the
    /// total maximum L1
    /// data size of the block. 0 means no maximum.
    #[method(name = "setMaxDASize")]
    async fn set_max_da_size(&self, max_tx_size: U64, max_block_size: U64) -> RpcResult<bool>;

    /// Returns the current maximum data availability size limits as (`max_tx_size`,
    /// `max_block_size`). Returns 0 for either value when no limit is set.
    #[method(name = "getMaxDASize")]
    async fn get_max_da_size(&self) -> RpcResult<(U64, U64)>;

    /// Sets the gas limit for future blocks produced by the miner.
    #[method(name = "setGasLimit")]
    async fn set_gas_limit(&self, gas_limit: U64) -> RpcResult<bool>;
}

base_metrics::define_metrics! {
    base_rpc.miner,
    struct = BaseMinerMetrics,
    #[describe("Max DA tx size set on the miner")]
    max_da_tx_size: gauge,
    #[describe("Max DA block size set on the miner")]
    max_da_block_size: gauge,
    #[describe("Gas limit set on the miner")]
    gas_limit: gauge,
}

/// Miner API extension for Base, exposing settings for the data availability configuration via the
/// `miner_` API.
#[derive(Debug, Clone)]
pub struct BaseMinerExtApi {
    da_config: BaseDAConfig,
    gas_limit_config: GasLimitConfig,
}

impl BaseMinerExtApi {
    /// Instantiate the miner API extension with the given, shareable data availability
    /// configuration.
    pub const fn new(da_config: BaseDAConfig, gas_limit_config: GasLimitConfig) -> Self {
        Self { da_config, gas_limit_config }
    }
}

#[async_trait]
impl MinerApiExtServer for BaseMinerExtApi {
    /// Handler for `miner_setMaxDASize` RPC method.
    async fn set_max_da_size(&self, max_tx_size: U64, max_block_size: U64) -> RpcResult<bool> {
        debug!(target: "rpc", max_tx_size = %max_tx_size, max_block_size = %max_block_size, "Setting max DA size");
        self.da_config.set_max_da_size(max_tx_size.to(), max_block_size.to());

        BaseMinerMetrics::max_da_tx_size().set(max_tx_size.to::<u64>() as f64);
        BaseMinerMetrics::max_da_block_size().set(max_block_size.to::<u64>() as f64);

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
        BaseMinerMetrics::gas_limit().set(gas_limit.to::<u64>() as f64);
        Ok(true)
    }
}
