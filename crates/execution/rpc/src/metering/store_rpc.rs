//! RPC extensions for the metering store.

use alloy_primitives::TxHash;
use base_common_types_payload::TransactionResult;
use base_execution_payload::SharedMeteringStore;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};
use tracing::info;

/// RPC trait for metering-related operations.
#[cfg_attr(not(test), rpc(server, namespace = "base"))]
#[cfg_attr(test, rpc(server, client, namespace = "base"))]
pub trait BaseApiExt {
    /// Sets metering information for a transaction.
    #[method(name = "setMeteringInformation")]
    async fn set_metering_information(
        &self,
        tx_hash: TxHash,
        meter: TransactionResult,
    ) -> RpcResult<()>;

    /// Enables or disables resource metering.
    #[method(name = "setMeteringEnabled")]
    async fn set_metering_enabled(&self, enabled: bool) -> RpcResult<()>;

    /// Clears all stored metering information.
    #[method(name = "clearMeteringInformation")]
    async fn clear_metering_information(&self) -> RpcResult<()>;
}

/// RPC extension wrapper around a [`SharedMeteringStore`].
#[derive(Debug)]
pub struct MeteringStoreExt {
    store: SharedMeteringStore,
}

impl MeteringStoreExt {
    /// Creates a new [`MeteringStoreExt`] with the given metering provider.
    pub fn new(store: SharedMeteringStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl BaseApiExtServer for MeteringStoreExt {
    async fn set_metering_information(
        &self,
        tx_hash: TxHash,
        metering: TransactionResult,
    ) -> RpcResult<()> {
        self.store.insert(tx_hash, metering);
        Ok(())
    }

    async fn set_metering_enabled(&self, enabled: bool) -> RpcResult<()> {
        self.store.set_enabled(enabled);
        Ok(())
    }

    async fn clear_metering_information(&self) -> RpcResult<()> {
        info!(
            rpc_method = "base_clearMeteringInformation",
            "Clearing builder metering information"
        );
        self.store.clear();
        Ok(())
    }
}
