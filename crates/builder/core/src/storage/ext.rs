//! RPC extensions for the transaction data store.

use std::time::Instant;

use alloy_primitives::TxHash;
use base_bundles::{AcceptedBundle, MeterBundleResponse};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};
use tracing::warn;

use super::TxDataStore;
use crate::BuilderMetrics;

#[cfg_attr(not(test), rpc(server, namespace = "base"))]
#[cfg_attr(test, rpc(server, client, namespace = "base"))]
pub trait BaseApiExt {
    #[method(name = "sendBackrunBundle")]
    async fn send_backrun_bundle(&self, bundle: AcceptedBundle) -> RpcResult<()>;

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
    metrics: BuilderMetrics,
}

impl TxDataStoreExt {
    pub fn new(store: TxDataStore) -> Self {
        Self { store, metrics: BuilderMetrics::default() }
    }
}

#[async_trait]
impl BaseApiExtServer for TxDataStoreExt {
    async fn send_backrun_bundle(&self, bundle: AcceptedBundle) -> RpcResult<()> {
        self.metrics.backrun_bundles_received_total.increment(1);

        let start = Instant::now();
        self.store.insert_backrun_bundle(bundle).map_err(|e| {
            warn!(target: "tx_data_store", error = %e, "Failed to store bundle");
            jsonrpsee::types::ErrorObject::owned(
                jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                format!("Failed to store bundle: {e}"),
                None::<()>,
            )
        })?;
        self.metrics.backrun_bundle_insert_duration.record(start.elapsed().as_secs_f64());

        Ok(())
    }

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
