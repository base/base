//! RPC extensions for the metering store.

use alloy_primitives::TxHash;
use base_builder_core::SharedMeteringProvider;
use base_bundles::MeterBundleResponse;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};

/// RPC trait for metering-related operations.
#[cfg_attr(not(test), rpc(server, namespace = "base"))]
#[cfg_attr(test, rpc(server, client, namespace = "base"))]
pub trait BaseApiExt {
    /// Sets metering information for a transaction.
    #[method(name = "setMeteringInformation")]
    async fn set_metering_information(
        &self,
        tx_hash: TxHash,
        meter: MeterBundleResponse,
    ) -> RpcResult<()>;

    /// Enables or disables resource metering.
    #[method(name = "setMeteringEnabled")]
    async fn set_metering_enabled(&self, enabled: bool) -> RpcResult<()>;

    /// Clears all stored metering information.
    #[method(name = "clearMeteringInformation")]
    async fn clear_metering_information(&self) -> RpcResult<()>;
}

/// RPC extension wrapper around a [`SharedMeteringProvider`].
#[derive(Debug)]
pub struct MeteringStoreExt {
    store: SharedMeteringProvider,
}

impl MeteringStoreExt {
    /// Creates a new [`MeteringStoreExt`] with the given metering provider.
    pub fn new(store: SharedMeteringProvider) -> Self {
        Self { store }
    }
}

#[async_trait]
impl BaseApiExtServer for MeteringStoreExt {
    async fn set_metering_information(
        &self,
        tx_hash: TxHash,
        metering: MeterBundleResponse,
    ) -> RpcResult<()> {
        self.store.insert(tx_hash, metering);
        Ok(())
    }

    async fn set_metering_enabled(&self, enabled: bool) -> RpcResult<()> {
        self.store.set_enabled(enabled);
        Ok(())
    }

    async fn clear_metering_information(&self) -> RpcResult<()> {
        self.store.clear();
        Ok(())
    }
}
