//! RPC extensions for the metering store.

use alloy_primitives::TxHash;
use base_builder_core::{
    BuilderMetrics, ResourceThrottleSchedule, SharedMeteringProvider, SharedResourceThrottleStore,
    VersionedResourceThrottleSchedule,
};
use base_bundles::MeterBundleResponse;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
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
        meter: MeterBundleResponse,
    ) -> RpcResult<()>;

    /// Enables or disables resource metering.
    #[method(name = "setMeteringEnabled")]
    async fn set_metering_enabled(&self, enabled: bool) -> RpcResult<()>;

    /// Clears all stored metering information.
    #[method(name = "clearMeteringInformation")]
    async fn clear_metering_information(&self) -> RpcResult<()>;
}

/// JWT-authenticated RPC methods for replacing the builder's resource-throttle schedule.
#[cfg_attr(not(test), rpc(server, namespace = "base"))]
#[cfg_attr(test, rpc(server, client, namespace = "base"))]
pub trait ResourceThrottleApi {
    /// Returns the active resource-throttle schedule and revision.
    #[method(name = "getResourceThrottleSchedule")]
    async fn get_resource_throttle_schedule(&self) -> RpcResult<VersionedResourceThrottleSchedule>;

    /// Atomically replaces the active resource-throttle schedule.
    #[method(name = "replaceResourceThrottleSchedule")]
    async fn replace_resource_throttle_schedule(
        &self,
        schedule: ResourceThrottleSchedule,
        expected_revision: Option<u64>,
    ) -> RpcResult<u64>;
}

/// RPC extension wrapper around a [`SharedMeteringProvider`].
#[derive(Debug)]
pub struct MeteringStoreExt {
    store: SharedMeteringProvider,
}

/// RPC extension for the JWT-authenticated resource-throttle control methods.
#[derive(Debug)]
pub struct ResourceThrottleExt {
    store: SharedResourceThrottleStore,
}

impl MeteringStoreExt {
    /// Creates a new [`MeteringStoreExt`] with the given metering provider.
    pub fn new(store: SharedMeteringProvider) -> Self {
        Self { store }
    }
}

impl ResourceThrottleExt {
    /// Creates a new resource-throttle RPC extension.
    pub const fn new(store: SharedResourceThrottleStore) -> Self {
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
        info!(
            rpc_method = "base_clearMeteringInformation",
            "Clearing builder metering information"
        );
        self.store.clear();
        Ok(())
    }
}

#[async_trait]
impl ResourceThrottleApiServer for ResourceThrottleExt {
    async fn get_resource_throttle_schedule(&self) -> RpcResult<VersionedResourceThrottleSchedule> {
        Ok(self.store.get())
    }

    async fn replace_resource_throttle_schedule(
        &self,
        schedule: ResourceThrottleSchedule,
        expected_revision: Option<u64>,
    ) -> RpcResult<u64> {
        let dimensions = schedule.dimensions.len();
        let revision = self.store.replace(schedule, expected_revision).map_err(|error| {
            ErrorObjectOwned::owned(ErrorCode::InvalidParams.code(), error.to_string(), None::<()>)
        })?;
        BuilderMetrics::resource_throttle_schedule_updates_total().increment(1);
        BuilderMetrics::resource_throttle_schedule_revision().set(revision as f64);
        info!(revision, dimensions, "resource throttle schedule replaced");
        Ok(revision)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base_builder_core::{
        NoopMeteringProvider, ResourceThrottleDimension, ResourceThrottleOperation,
        ResourceThrottleStore,
    };

    use super::*;

    #[tokio::test]
    async fn resource_throttle_rpc_replaces_schedule_with_revision_check() {
        let store = Arc::new(ResourceThrottleStore::default());
        let extension = ResourceThrottleExt::new(Arc::clone(&store));
        let schedule = ResourceThrottleSchedule {
            dimensions: vec![ResourceThrottleDimension {
                name: "execution".to_string(),
                block_limit: 100,
                transaction_limit: None,
                base_gas_weight: 1,
                operations: vec![ResourceThrottleOperation {
                    name: "SSTORE".to_string(),
                    gas_used_weight: 2,
                    count_cost: 0,
                }],
            }],
            ..Default::default()
        };

        assert_eq!(extension.get_resource_throttle_schedule().await.unwrap().revision, 0);
        assert_eq!(
            extension.replace_resource_throttle_schedule(schedule, Some(0)).await.unwrap(),
            1
        );
        assert_eq!(store.revision(), 1);
        assert!(
            extension
                .replace_resource_throttle_schedule(ResourceThrottleSchedule::default(), Some(0))
                .await
                .is_err()
        );
    }

    #[test]
    fn resource_throttle_methods_are_separate_from_regular_metering_methods() {
        let regular = MeteringStoreExt::new(Arc::new(NoopMeteringProvider)).into_rpc();
        assert!(!regular.method_names().any(|name| name == "base_getResourceThrottleSchedule"));

        let authenticated =
            ResourceThrottleExt::new(Arc::new(ResourceThrottleStore::default())).into_rpc();
        assert!(
            authenticated.method_names().any(|name| name == "base_getResourceThrottleSchedule")
        );
        assert!(
            authenticated.method_names().any(|name| name == "base_replaceResourceThrottleSchedule")
        );
    }
}
