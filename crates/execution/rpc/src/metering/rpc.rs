//! Implementation of the metering RPC API.

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_common_types_chain::BaseBlock;
use base_execution_payload::{MeterBlockResponse, meter_block};
use base_execution_state_provider::{BlockReader, BlockReaderIdExt, ChainSpecProvider};
use jsonrpsee::core::{RpcResult, async_trait};
use tracing::{debug, error};

use super::MeteringApiServer;

/// Implementation of the metering RPC API.
pub struct MeteringApiImpl {
    provider: base_execution_state_provider::BlockchainProvider,
}

impl std::fmt::Debug for MeteringApiImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeteringApiImpl").finish_non_exhaustive()
    }
}

impl MeteringApiImpl {
    /// Creates a new instance of `MeteringApi`.
    pub const fn new(provider: base_execution_state_provider::BlockchainProvider) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl MeteringApiServer for MeteringApiImpl {
    async fn meter_block_by_hash(&self, hash: B256) -> RpcResult<MeterBlockResponse> {
        debug!(block_hash = %hash, "Starting block metering by hash");

        let block = self
            .provider
            .block_by_hash(hash)
            .map_err(|e| {
                error!(error = %e, "Failed to get block by hash");
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    format!("Failed to get block: {e}"),
                    None::<()>,
                )
            })?
            .ok_or_else(|| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InvalidParams.code(),
                    format!("Block not found: {hash}"),
                    None::<()>,
                )
            })?;

        let response = self.meter_block_internal(&block)?;

        debug!(
            block_hash = %hash,
            signer_recovery_time_us = response.signer_recovery_time_us,
            execution_time_us = response.execution_time_us,
            total_time_us = response.total_time_us,
            "Block metering completed successfully"
        );

        Ok(response)
    }

    async fn meter_block_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<MeterBlockResponse> {
        debug!(block_number = ?number, "Starting block metering by number");

        let block = self
            .provider
            .block_by_number_or_tag(number)
            .map_err(|e| {
                error!(error = %e, "Failed to get block by number");
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    format!("Failed to get block: {e}"),
                    None::<()>,
                )
            })?
            .ok_or_else(|| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InvalidParams.code(),
                    format!("Block not found: {number:?}"),
                    None::<()>,
                )
            })?;

        let response = self.meter_block_internal(&block)?;

        debug!(
            block_number = ?number,
            block_hash = %response.block_hash,
            signer_recovery_time_us = response.signer_recovery_time_us,
            execution_time_us = response.execution_time_us,
            total_time_us = response.total_time_us,
            "Block metering completed successfully"
        );

        Ok(response)
    }
}

impl MeteringApiImpl {
    /// Internal helper to meter a block's execution
    fn meter_block_internal(&self, block: &BaseBlock) -> RpcResult<MeterBlockResponse> {
        meter_block(self.provider.clone(), self.provider.chain_spec(), block).map_err(|e| {
            error!(error = %e, "Block metering failed");
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InternalError.code(),
                format!("Block metering failed: {e}"),
                None::<()>,
            )
        })
    }
}
