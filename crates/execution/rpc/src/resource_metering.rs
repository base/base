//! RPC for native payload resource metering and throttling.

use alloy_primitives::TxHash;
use base_bundles::MeterBundleResponse;
use base_execution_payload_builder::{
    ResourceMeteringMetrics, ResourceMeteringSchedule, SharedMeteringProvider,
    SharedResourceMeteringStore, VersionedResourceMeteringSchedule,
};
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::{RpcResult, async_trait};
use jsonrpsee_types::{ErrorCode, ErrorObjectOwned};
use tracing::info;

/// RPC trait for `meterBundle` result ingestion used by native payload admission.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "base"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "base"))]
pub trait ResourceMeteringApi {
    /// Sets metering information for a transaction.
    #[method(name = "setMeteringInformation")]
    async fn set_metering_information(
        &self,
        tx_hash: TxHash,
        meter: MeterBundleResponse,
    ) -> RpcResult<()>;

    /// Enables or disables resource metering lookups.
    #[method(name = "setMeteringEnabled")]
    async fn set_metering_enabled(&self, enabled: bool) -> RpcResult<()>;

    /// Clears all stored metering information.
    #[method(name = "clearMeteringInformation")]
    async fn clear_metering_information(&self) -> RpcResult<()>;
}

/// JWT-authenticated RPC methods for replacing the native builder's resource-metering schedule.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "base"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "base"))]
pub trait ResourceMeteringScheduleApi {
    /// Returns the active resource-metering schedule and revision.
    #[method(name = "getResourceMeteringSchedule")]
    async fn get_resource_metering_schedule(&self) -> RpcResult<VersionedResourceMeteringSchedule>;

    /// Atomically replaces the active resource-metering schedule.
    #[method(name = "replaceResourceMeteringSchedule")]
    async fn replace_resource_metering_schedule(
        &self,
        schedule: ResourceMeteringSchedule,
        expected_revision: Option<u64>,
    ) -> RpcResult<u64>;
}

/// RPC extension wrapper around a [`SharedMeteringProvider`].
#[derive(Debug)]
pub struct ResourceMeteringApiExt {
    store: SharedMeteringProvider,
}

impl ResourceMeteringApiExt {
    /// Creates a new [`ResourceMeteringApiExt`] with the given metering provider.
    pub fn new(store: SharedMeteringProvider) -> Self {
        Self { store }
    }
}

/// RPC extension for the JWT-authenticated resource-metering schedule methods.
#[derive(Debug)]
pub struct ResourceMeteringScheduleExt {
    store: SharedResourceMeteringStore,
}

impl ResourceMeteringScheduleExt {
    /// Creates a new resource-metering schedule RPC extension.
    pub const fn new(store: SharedResourceMeteringStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ResourceMeteringApiServer for ResourceMeteringApiExt {
    async fn set_metering_information(
        &self,
        tx_hash: TxHash,
        metering: MeterBundleResponse,
    ) -> RpcResult<()> {
        base_execution_payload_builder::MeteringProvider::insert(
            self.store.as_ref(),
            tx_hash,
            metering,
        );
        Ok(())
    }

    async fn set_metering_enabled(&self, enabled: bool) -> RpcResult<()> {
        base_execution_payload_builder::MeteringProvider::set_enabled(self.store.as_ref(), enabled);
        Ok(())
    }

    async fn clear_metering_information(&self) -> RpcResult<()> {
        info!(
            rpc_method = "base_clearMeteringInformation",
            "Clearing payload metering information"
        );
        base_execution_payload_builder::MeteringProvider::clear(self.store.as_ref());
        Ok(())
    }
}

#[async_trait]
impl ResourceMeteringScheduleApiServer for ResourceMeteringScheduleExt {
    async fn get_resource_metering_schedule(&self) -> RpcResult<VersionedResourceMeteringSchedule> {
        Ok(self.store.get())
    }

    async fn replace_resource_metering_schedule(
        &self,
        schedule: ResourceMeteringSchedule,
        expected_revision: Option<u64>,
    ) -> RpcResult<u64> {
        let dimensions = schedule.dimensions.len();
        let revision = self.store.replace(schedule, expected_revision).map_err(|error| {
            ErrorObjectOwned::owned(ErrorCode::InvalidParams.code(), error.to_string(), None::<()>)
        })?;
        ResourceMeteringMetrics::schedule_updates_total().increment(1);
        ResourceMeteringMetrics::record_schedule_revision(revision);
        info!(revision, dimensions, "resource metering schedule replaced");
        Ok(revision)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base_execution_payload_builder::{
        NoopMeteringProvider, ResourceMeteringDimension, ResourceMeteringOperation,
        ResourceMeteringStore,
    };

    use super::*;

    #[tokio::test]
    async fn resource_metering_rpc_replaces_schedule_with_revision_check() {
        let store = Arc::new(ResourceMeteringStore::default());
        let extension = ResourceMeteringScheduleExt::new(Arc::clone(&store));
        let schedule = ResourceMeteringSchedule {
            dimensions: vec![ResourceMeteringDimension {
                name: "execution".to_string(),
                block_limit: 100,
                transaction_limit: None,
                base_gas_weight: 1,
                operations: vec![ResourceMeteringOperation {
                    name: "SSTORE".to_string(),
                    gas_used_weight: 2,
                    count_cost: 0,
                }],
            }],
            ..Default::default()
        };

        assert_eq!(extension.get_resource_metering_schedule().await.unwrap().revision, 0);
        assert_eq!(
            extension.replace_resource_metering_schedule(schedule, Some(0)).await.unwrap(),
            1
        );
        assert_eq!(store.revision(), 1);
        assert!(
            extension
                .replace_resource_metering_schedule(ResourceMeteringSchedule::default(), Some(0))
                .await
                .is_err()
        );
    }

    #[test]
    fn schedule_methods_are_separate_from_metering_ingestion() {
        let regular = ResourceMeteringApiExt::new(Arc::new(NoopMeteringProvider)).into_rpc();
        assert!(!regular.method_names().any(|name| name == "base_getResourceMeteringSchedule"));

        let authenticated =
            ResourceMeteringScheduleExt::new(Arc::new(ResourceMeteringStore::default())).into_rpc();
        assert!(
            authenticated.method_names().any(|name| name == "base_getResourceMeteringSchedule")
        );
        assert!(
            authenticated.method_names().any(|name| name == "base_replaceResourceMeteringSchedule")
        );
    }
}
