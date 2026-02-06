//! RPC extensions for the transaction data store.

use alloy_primitives::TxHash;
use base_bundles::MeterBundleResponse;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};

use super::TxDataStore;

#[cfg_attr(not(test), rpc(server, namespace = "base"))]
#[cfg_attr(test, rpc(server, client, namespace = "base"))]
pub trait BaseApiExt {
    #[method(name = "setMeteringInformation")]
    async fn set_metering_information(
        &self,
        tx_hash: TxHash,
        meter: MeterBundleResponse,
    ) -> RpcResult<()>;

    #[method(name = "setMeteringEnabled")]
    async fn set_metering_enabled(&self, enabled: bool) -> RpcResult<()>;

    #[method(name = "clearMeteringInformation")]
    async fn clear_metering_information(&self) -> RpcResult<()>;
}

#[derive(Debug)]
pub struct TxDataStoreExt {
    store: TxDataStore,
}

impl TxDataStoreExt {
    pub const fn new(store: TxDataStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl BaseApiExtServer for TxDataStoreExt {
    async fn set_metering_information(
        &self,
        tx_hash: TxHash,
        metering: MeterBundleResponse,
    ) -> RpcResult<()> {
        self.store.insert_metering(tx_hash, metering);
        Ok(())
    }

    async fn set_metering_enabled(&self, enabled: bool) -> RpcResult<()> {
        self.store.set_metering_enabled(enabled);
        Ok(())
    }

    async fn clear_metering_information(&self) -> RpcResult<()> {
        self.store.clear_metering();
        Ok(())
    }
}
